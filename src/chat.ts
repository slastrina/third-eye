// UI side of the chat IPC surface defined in src-tauri/src/llm/commands.rs.
// The event names and payload shapes are the contract; keep them in sync with
// TOKEN_EVENT/DONE_EVENT/ERROR_EVENT and their serde camelCase serialization.
//
// All chat state transitions live in the pure `chatReducer` so stale-event
// filtering, pre-resolve buffering, and every failure path are unit-testable
// without a Tauri runtime (src/chat.test.ts). App.tsx is only glue.

import { invoke } from "@tauri-apps/api/core";
import { describeCall } from "./action-labels";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export const TOKEN_EVENT = "llm://token";
export const DONE_EVENT = "llm://done";
export const ERROR_EVENT = "llm://error";
/** Reasoning-delta stream (thinking models): a model's chain-of-thought arrives
 *  on its own event so the overlay renders a dimmed "Thinking…" region distinct
 *  from the answer — and reasoning never lands in the answer body (which used to
 *  fill with blank newlines while the heavy model thought). Keep in sync with
 *  REASONING_EVENT in src-tauri/src/llm/commands.rs (pinned by a Rust test and
 *  its TS twin). Transient: cleared per turn, never persisted. */
export const REASONING_EVENT = "llm://reasoning";
/** Tool-phase events (S03): emitted by the backend dispatch loop when the
 *  model requests a tool and when its execution settles. These drive the
 *  memory-consulted indicator — the UI-facing observability surface. Keep in
 *  sync with TOOL_CALL_EVENT/TOOL_RESULT_EVENT in src-tauri/src/llm/toolloop.rs. */
export const TOOL_CALL_EVENT = "llm://tool-call";
export const TOOL_RESULT_EVENT = "llm://tool-result";
/** The one tool S03 ships; results under this name flip the indicator. */
export const MEMORY_SEARCH_TOOL = "memory_search";
/** The terminal tool (computer-control I2) — Rust's RUN_COMMAND_TOOL twin. */
export const RUN_COMMAND_TOOL = "run_command";
/** The workspace exec tool (coding-agent S4) — Rust's RUN_IN_WORKSPACE_TOOL
 *  twin. Rendered in the same transcript terminal block as run_command. */
export const RUN_IN_WORKSPACE_TOOL = "run_in_workspace";
/** The workspace diff tool (coding-agent S5) — Rust's WORKSPACE_DIFF_TOOL
 *  twin. Rendered as the transcript's collapsible colored diff block. */
export const WORKSPACE_DIFF_TOOL = "workspace_diff";
/** Live output of a running run_in_workspace command (coding-agent S4):
 *  each stdout/stderr chunk streams into the terminal block as it happens.
 *  Keep in sync with TERMINAL_CHUNK_EVENT in src-tauri/src/llm/commands.rs. */
export const TERMINAL_CHUNK_EVENT = "llm://terminal-chunk";
/** Routing-state broadcast (S07): mutation responses only reach the calling
 *  window, so the backend emits the updated ModelInfo app-wide after every
 *  successful set_model / set_lane_model. The overlay consumes this to stay
 *  truthful when the settings window changes routing. */
export const MODEL_INFO_EVENT = "llm://model-info";
/** Per-request routed lane (`llm://routed`) — what auto actually picked. */
export const ROUTED_EVENT = "llm://routed";

export interface RoutedPayload {
  requestId: number;
  lane: string;
  model: string;
}

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

/** llm://reasoning payload — the serde camelCase serialization of Rust's
 *  ReasoningEvent. `delta` is one chain-of-thought fragment to append to the
 *  transient Thinking… region. */
export interface ReasoningPayload {
  requestId: number;
  delta: string;
}

export interface DonePayload {
  requestId: number;
  text: string;
  tokenCount: number;
  /** Real token spend for the whole run (summed across tool rounds);
   *  null when the backend reports no usage. */
  promptTokens?: number | null;
  completionTokens?: number | null;
  firstTokenMs: number | null;
  totalMs: number;
}

/** Kind-tagged error JSON — the serde serialization of Rust's LlmError.
 *  "tools-unsupported" is a 4xx rejection of a tools-carrying request whose
 *  body names tools: the loaded model can't call tools, distinct from
 *  "no-model" so the banner can say so instead of "no model loaded".
 *  "guard-blocked" is the privacy guard refusing to send a request to a
 *  non-loopback endpoint (R016 fail closed): it carries a kebab-case
 *  machine-readable `reason` instead of free-text `detail` — never any
 *  request text. */
export type LlmError =
  | { kind: "offline"; endpoint: string; detail: string }
  | { kind: "no-model"; endpoint: string; detail: string }
  | { kind: "tools-unsupported"; endpoint: string; detail: string }
  | { kind: "interrupted"; endpoint: string; partialText: string; detail: string }
  | { kind: "guard-blocked"; endpoint: string; reason: string }
  | { kind: "empty-completion"; endpoint: string; detail: string };

export interface ErrorPayload {
  requestId: number;
  error: LlmError;
}

/** One complete tool call the model requested — the serde camelCase
 *  serialization of Rust's ToolCall. `arguments` is the raw JSON string
 *  exactly as the model produced it. */
export interface ToolCall {
  id: string;
  name: string;
  arguments: string;
}

/** How a memory search ranked its results (S02 degrade contract). */
export type SearchMode = "semantic" | "keyword";

/** llm://tool-call payload: a model-requested call about to execute. */
export interface ToolCallPayload {
  requestId: number;
  round: number;
  call: ToolCall;
}

/** llm://tool-result payload: one executed call's outcome. `ok: false`
 *  carries the typed failure kind; a successful memory search carries its
 *  result count and ranking mode. */
export interface ToolResultPayload {
  requestId: number;
  round: number;
  callId: string;
  name: string;
  ok: boolean;
  resultCount: number | null;
  mode: SearchMode | null;
  failure: string | null;
  /** Bounded output preview — present only on run_command results (the
   *  chat terminal block's content; serde skips it elsewhere). */
  preview?: string | null;
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
  /** AUTO routing mode (coding-agent S1): requests pick their own lane;
   *  the chips become manual overrides. */
  auto: boolean;
}

/** Start a streaming completion; resolves to the request id whose llm://*
 *  events to accept. The backend aborts any prior in-flight request. */
export function sendChat(messages: ChatMessage[]): Promise<number> {
  return invoke<number>("chat", { messages });
}

/** Which build is running (Settings About row): version, commit, built-at. */
export interface BuildInfo {
  version: string;
  gitHash: string;
  builtAtMs: number;
}

export function buildInfo(): Promise<BuildInfo> {
  return invoke<BuildInfo>("build_info");
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
export function onRouted(cb: (payload: RoutedPayload) => void): Promise<UnlistenFn> {
  return listen<RoutedPayload>(ROUTED_EVENT, (e) => cb(e.payload));
}

export function onModelInfoBroadcast(cb: (info: ModelInfo) => void): Promise<UnlistenFn> {
  return listen<ModelInfo>(MODEL_INFO_EVENT, (e) => cb(e.payload));
}

export function onLlmToken(cb: (payload: TokenPayload) => void): Promise<UnlistenFn> {
  return listen<TokenPayload>(TOKEN_EVENT, (e) => cb(e.payload));
}

export function onLlmReasoning(cb: (payload: ReasoningPayload) => void): Promise<UnlistenFn> {
  return listen<ReasoningPayload>(REASONING_EVENT, (e) => cb(e.payload));
}

export function onLlmDone(cb: (payload: DonePayload) => void): Promise<UnlistenFn> {
  return listen<DonePayload>(DONE_EVENT, (e) => cb(e.payload));
}

export function onLlmError(cb: (payload: ErrorPayload) => void): Promise<UnlistenFn> {
  return listen<ErrorPayload>(ERROR_EVENT, (e) => cb(e.payload));
}

export function onLlmToolCall(cb: (payload: ToolCallPayload) => void): Promise<UnlistenFn> {
  return listen<ToolCallPayload>(TOOL_CALL_EVENT, (e) => cb(e.payload));
}

export function onLlmToolResult(cb: (payload: ToolResultPayload) => void): Promise<UnlistenFn> {
  return listen<ToolResultPayload>(TOOL_RESULT_EVENT, (e) => cb(e.payload));
}

/** The llm://terminal-chunk payload — serde camelCase of Rust's
 *  TerminalChunkPayload. */
export interface TerminalChunkPayload {
  requestId: number;
  callId: string;
  chunk: string;
}

export function onTerminalChunk(cb: (payload: TerminalChunkPayload) => void): Promise<UnlistenFn> {
  return listen<TerminalChunkPayload>(TERMINAL_CHUNK_EVENT, (e) => cb(e.payload));
}

/** Workspace-roots broadcast (`workspace://roots`): fired on every change —
 *  Settings edits AND bridge adds (CLI / Finder Quick Action) — so the
 *  overlay's workspace chip stays truthful without polling. */
export const WORKSPACE_ROOTS_EVENT = "workspace://roots";

export function onWorkspaceRoots(cb: (status: WorkspaceStatus) => void): Promise<UnlistenFn> {
  return listen<WorkspaceStatus>(WORKSPACE_ROOTS_EVENT, (e) => cb(e.payload));
}

/** VS Code bridge snapshot (coding-agent S7) — serde camelCase of Rust's
 *  BridgeStatus. Health-as-value, never an error. */
export interface BridgeStatus {
  running: boolean;
  port: number | null;
  connected: number;
  discoveryPath: string | null;
  vscodeDetected: boolean;
}

export function bridgeStatus(): Promise<BridgeStatus> {
  return invoke<BridgeStatus>("bridge_status");
}

/** Background-phase status (2026-08-02): what the run is waiting on when
 *  nothing is streaming. Keep in sync with PHASE_EVENT in Rust. */
export const PHASE_EVENT = "llm://phase";

/** The llm://phase payload — serde camelCase of Rust's PhasePayload. */
export interface PhasePayload {
  requestId: number;
  /** "loading-model" | "processing-prompt" */
  phase: string;
  model: string | null;
  waitedMs: number;
  detail: string | null;
}

export function onLlmPhase(cb: (payload: PhasePayload) => void): Promise<UnlistenFn> {
  return listen<PhasePayload>(PHASE_EVENT, (e) => cb(e.payload));
}

/** Verbose status-line mode: broadcast so the overlay applies a Settings
 *  toggle live. Keep in sync with VERBOSE_STATUS_EVENT in Rust. */
export const VERBOSE_STATUS_EVENT = "settings://verbose-status";

export interface VerboseStatus {
  enabled: boolean;
  error: string | null;
}

export function verboseStatus(): Promise<VerboseStatus> {
  return invoke<VerboseStatus>("verbose_status");
}

export function setVerboseStatus(enable: boolean): Promise<VerboseStatus> {
  return invoke<VerboseStatus>("set_verbose_status", { enable });
}

export function onVerboseStatus(cb: (status: VerboseStatus) => void): Promise<UnlistenFn> {
  return listen<VerboseStatus>(VERBOSE_STATUS_EVENT, (e) => cb(e.payload));
}

/** Teach Me mode (2026-08-18): human-way keyboard/mouse only, narrated,
 *  ending in a do-it-yourself recap — the shortcut tools are structurally
 *  stripped backend-side. Same status shape as verbose. Keep in sync with
 *  TEACH_MODE_EVENT in Rust. */
export const TEACH_MODE_EVENT = "settings://teach-mode";

export function teachMode(): Promise<VerboseStatus> {
  return invoke<VerboseStatus>("teach_mode");
}

export function setTeachMode(enable: boolean): Promise<VerboseStatus> {
  return invoke<VerboseStatus>("set_teach_mode", { enable });
}

export function onTeachMode(cb: (status: VerboseStatus) => void): Promise<UnlistenFn> {
  return listen<VerboseStatus>(TEACH_MODE_EVENT, (e) => cb(e.payload));
}

/** One served model with LM Studio's native detail — serde camelCase of
 *  Rust's LmModelRow (empty list when the endpoint is not LM Studio). */
export interface LmModelRow {
  id: string;
  state: string;
  toolUse: boolean;
  quantization: string | null;
  maxContextLength: number | null;
}

export function listModelsDetailed(): Promise<LmModelRow[]> {
  return invoke<LmModelRow[]>("list_models_detailed");
}

/** Compact token count: 842 → "842", 6377 → "6.4k", 123456 → "123k". */
export function formatTokens(count: number): string {
  if (count < 1000) return String(count);
  if (count < 100_000) return `${(count / 1000).toFixed(1).replace(/\.0$/, "")}k`;
  return `${Math.round(count / 1000)}k`;
}

/** Human copy for one wait phase. Basic mode: plain words; verbose adds
 *  the model, wait timer, and raw detail. Pure (unit-tested). */
export function phaseStatusLine(phase: PhasePayload, verbose: boolean): string {
  const seconds = Math.max(1, Math.round(phase.waitedMs / 1000));
  const basic =
    phase.phase === "loading-model"
      ? "Loading the model…"
      : "Reading your request…";
  if (!verbose) return basic;
  const parts = [
    basic,
    phase.model ?? "default model",
    `waiting ${seconds}s`,
  ];
  if (phase.detail) parts.push(phase.detail);
  return parts.join(" · ");
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

/** Strip a `data:image/...;base64,` prefix down to the raw base64 payload
 *  (what CapturedFrame carries). Pure; null when the URL is not a base64
 *  data URL (a pasted SVG-as-text, a file:// path — refuse, never send
 *  garbage bytes as a "PNG"). */
export function base64FromDataUrl(dataUrl: string): string | null {
  const match = /^data:image\/[a-z+.-]+;base64,(.+)$/i.exec(dataUrl);
  return match ? match[1] : null;
}

/** Longest edge a pasted image may keep — larger pastes are downscaled
 *  (vision models cap resolution anyway; a 5K screenshot as base64 would
 *  bloat the request for nothing). */
export const PASTE_MAX_DIMENSION = 2048;

/** Scale (w,h) to fit PASTE_MAX_DIMENSION, preserving aspect. Pure. */
export function pasteScaledSize(width: number, height: number): { width: number; height: number } {
  const longest = Math.max(width, height);
  if (longest <= PASTE_MAX_DIMENSION) return { width, height };
  const factor = PASTE_MAX_DIMENSION / longest;
  return {
    width: Math.max(1, Math.round(width * factor)),
    height: Math.max(1, Math.round(height * factor)),
  };
}

/** Decode a pasted image file into a CapturedFrame (N1, spec 2026-08-02):
 *  re-encoded to PNG via canvas (normalizes JPEG/WebP/TIFF pastes),
 *  downscaled past PASTE_MAX_DIMENSION. Rides the SAME attachment pipeline
 *  as the screenshot button — transient, never persisted (R011). */
export function frameFromImageFile(file: File): Promise<CapturedFrame> {
  return new Promise((resolve, reject) => {
    const url = URL.createObjectURL(file);
    const image = new Image();
    image.onload = () => {
      URL.revokeObjectURL(url);
      const { width, height } = pasteScaledSize(image.naturalWidth, image.naturalHeight);
      const canvas = document.createElement("canvas");
      canvas.width = width;
      canvas.height = height;
      const context = canvas.getContext("2d");
      if (!context) {
        reject(new Error("canvas 2d context unavailable"));
        return;
      }
      context.drawImage(image, 0, 0, width, height);
      const base64Png = base64FromDataUrl(canvas.toDataURL("image/png"));
      if (!base64Png) {
        reject(new Error("pasted image could not be encoded as PNG"));
        return;
      }
      resolve({ width, height, base64Png });
    };
    image.onerror = () => {
      URL.revokeObjectURL(url);
      reject(new Error("pasted data is not a decodable image"));
    };
    image.src = url;
  });
}

/** One attached text-file context (2026-08-02): a file the user explicitly
 *  picked to ground this chat turn. Transient — rides the outgoing wire
 *  turn, never stored (R011). */
export interface FileContext {
  name: string;
  path: string;
  text: string;
}

/** Cap per attached file — the content rides the prompt. */
export const FILE_CONTEXT_MAX_CHARS = 48_000;

/** Wire-side context blocks appended to the user's question: each attached
 *  file as a fenced block with its name/path. Pure (unit-tested); empty
 *  input adds nothing. */
export function composeContextBlocks(files: FileContext[]): string {
  return files
    .map((file) => {
      const total = file.text.length;
      const shown =
        total > FILE_CONTEXT_MAX_CHARS
          ? `${file.text.slice(0, FILE_CONTEXT_MAX_CHARS)}
[…truncated — ${total} chars total]`
          : file.text;
      return `

[Attached file: ${file.name} (${file.path})]
\`\`\`
${shown}
\`\`\``;
    })
    .join("");
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
// Nudge IPC (S05) — contract defined in src-tauri/src/nudge
// ---------------------------------------------------------------------------

/** Nudge lifecycle events. Keep in sync with SHOW_EVENT/DISMISS_EVENT/
 *  STATE_EVENT in src-tauri/src/nudge/mod.rs (pinned by a Rust test). */
export const NUDGE_SHOW_EVENT = "nudge://show";
export const NUDGE_DISMISS_EVENT = "nudge://dismiss";
export const NUDGE_STATE_EVENT = "nudge://state";

/** The `nudge://show` payload — the serde camelCase serialization of Rust's
 *  NudgePayload. Pixel-free by construction: text, app context, timestamps,
 *  and memory-context strings only. */
export interface NudgePayload {
  /** Always "nudge" today; lets the UI switch on kind if later slices add
   *  more overlay callouts. */
  kind: string;
  /** The one-line banner message from the classifier. */
  message: string;
  /** Text of the triggering observation — the screen context a
   *  summon-from-nudge chat is grounded in. */
  screenText: string;
  appContext: string | null;
  capturedAtMs: number;
  /** Relevant memory summaries fetched at classification time; empty when
   *  the memory search degraded. */
  memoryContext: string[];
}

/** Why the active nudge went away — the `nudge://dismiss` payload (Rust's
 *  DismissReason, kebab-case). "summoned" is the one that stages the chat
 *  context preload. */
export type NudgeDismissReason = "auto-timeout" | "summoned" | "disabled" | "hidden";

/** Per-reason suppression counters riding NudgeStatus (observability: "why
 *  has it never nudged me" is answerable from status alone). */
export interface NudgeSuppressedCounts {
  disabled: number;
  overlayVisible: number;
  coolingDown: number;
  emptyBatch: number;
}

/** The `nudge_status` / `nudge://state` shape — health-as-value, never an
 *  IPC error; persist failures and classification errors ride the fields. */
export interface NudgeStatus {
  enabled: boolean;
  active: boolean;
  lastNudgeAtMs: number | null;
  lastError: LlmError | null;
  suppressed: NudgeSuppressedCounts;
  persistError: string | null;
  /** Live tunables (Settings chips, 2026-07-27). */
  cooldownSecs: number;
  autoDismissSecs: number;
}

/** One shown nudge in the Settings history list (pixel-free). */
export interface NudgeHistoryEntry {
  message: string;
  appContext: string | null;
  shownAtMs: number;
  dismissReason: string | null;
}

/** Current nudge state (health-as-value, like `watcher_status`). */
export function nudgeStatus(): Promise<NudgeStatus> {
  return invoke<NudgeStatus>("nudge_status");
}

/** Set the nudges toggle. Never rejects backend-side — a persist failure
 *  rides `persistError` on the returned authoritative status. */
export function setNudgesEnabled(enable: boolean): Promise<NudgeStatus> {
  return invoke<NudgeStatus>("set_nudges_enabled", { enable });
}

/** Set the nudge cooldown (closed choice set backend-side; out-of-set
 *  values return the unchanged authoritative status). */
export function setNudgeCooldown(secs: number): Promise<NudgeStatus> {
  return invoke<NudgeStatus>("set_nudge_cooldown", { secs });
}

/** Set the banner auto-dismiss window (applies to the next nudge). */
export function setNudgeAutoDismiss(secs: number): Promise<NudgeStatus> {
  return invoke<NudgeStatus>("set_nudge_auto_dismiss", { secs });
}

/** Recently shown nudges, newest first (bounded backend-side). */
export function nudgeHistory(): Promise<NudgeHistoryEntry[]> {
  return invoke<NudgeHistoryEntry[]>("nudge_history");
}

// ---------------------------------------------------------------------------
// Per-tool switchboard IPC — contract in src-tauri/src/tool_toggles.rs
// ---------------------------------------------------------------------------

/** One built-in tool row in Settings (camelCase serde of ToolToggleRow). */
export interface ToolToggleRow {
  name: string;
  label: string;
  description: string;
  enabled: boolean;
}

/** The `tool_toggles_status` / `set_tool_enabled` shape — health-as-value. */
export interface ToolTogglesStatus {
  tools: ToolToggleRow[];
  persistError: string | null;
}

/** Workspace roots (coding-agent S2): the only folders the coding tools
 *  may touch. */
export interface WorkspaceStatus {
  roots: string[];
  persistError: string | null;
}

export function workspaceRoots(): Promise<WorkspaceStatus> {
  return invoke<WorkspaceStatus>("workspace_roots");
}

export function setWorkspaceRoots(roots: string[]): Promise<WorkspaceStatus> {
  return invoke<WorkspaceStatus>("set_workspace_roots", { roots });
}

export function toolTogglesStatus(): Promise<ToolTogglesStatus> {
  return invoke<ToolTogglesStatus>("tool_toggles_status");
}

/** Flip one tool. Never rejects backend-side — a persist failure rides
 *  `persistError` on the returned authoritative status. */
export function setToolEnabled(name: string, enable: boolean): Promise<ToolTogglesStatus> {
  return invoke<ToolTogglesStatus>("set_tool_enabled", { name, enable });
}

/** The screenshot taken when the nudge stamped `capturedAtMs` was shown
 *  (base64 PNG), or null when none was retained (privacy mode, capture
 *  failure, superseded by a newer nudge). */
export function nudgeContextFrame(capturedAtMs: number): Promise<string | null> {
  return invoke<string | null>("nudge_context_frame", { capturedAtMs });
}

/** How long a dismissed nudge's context keeps grounding the next question.
 *  The banner auto-dismisses after ~12s, but the user often summons chat
 *  well after that — within this window the summon still knows what the
 *  nudge was about; past it, a new chat starts clean rather than dragging
 *  in a stale screen. */
export const NUDGE_PRELOAD_FRESH_MS = 5 * 60_000;

/** The staged preload if it is still fresh at submit time, else null. */
export function freshNudgePreload(
  preload: NudgePayload | null,
  nowMs: number,
): NudgePayload | null {
  if (preload === null) return null;
  return nowMs - preload.capturedAtMs <= NUDGE_PRELOAD_FRESH_MS ? preload : null;
}

/** How the staged nudge context is shown in the composer's context row —
 *  the user can SEE that the next question is grounded in what Third Eye
 *  nudged them about (message, app, how long ago), not guess at it. Pure. */
export function nudgeChipLabel(payload: NudgePayload, nowMs: number): string {
  const ageMs = Math.max(0, nowMs - payload.capturedAtMs);
  const age =
    ageMs < 15_000
      ? "just now"
      : ageMs < 60_000
        ? `${Math.round(ageMs / 1000)}s ago`
        : `${Math.round(ageMs / 60_000)}m ago`;
  const where = payload.appContext ? ` · ${payload.appContext}` : "";
  return `nudge · ${payload.message}${where} · ${age}`;
}

/** The chip's hover detail: the screen text the nudge was grounded in,
 *  clipped so a tooltip stays a tooltip. */
export function nudgeChipDetail(payload: NudgePayload, maxChars = 400): string {
  const text = payload.screenText.trim().replace(/\s+/g, " ");
  const clipped = text.length > maxChars ? `${text.slice(0, maxChars)}…` : text;
  return `Grounds your next question in what was on screen when Third Eye nudged you${payload.appContext ? ` (${payload.appContext})` : ""}:\n${clipped}`;
}

export function onNudgeShow(cb: (payload: NudgePayload) => void): Promise<UnlistenFn> {
  return listen<NudgePayload>(NUDGE_SHOW_EVENT, (e) => cb(e.payload));
}

export function onNudgeDismiss(cb: (reason: NudgeDismissReason) => void): Promise<UnlistenFn> {
  return listen<NudgeDismissReason>(NUDGE_DISMISS_EVENT, (e) => cb(e.payload));
}

/** Subscribe to the app-wide nudge status broadcast (`nudge://state`). */
export function onNudgeState(cb: (status: NudgeStatus) => void): Promise<UnlistenFn> {
  return listen<NudgeStatus>(NUDGE_STATE_EVENT, (e) => cb(e.payload));
}

// ---------------------------------------------------------------------------
// HID arming IPC (S03/M005) — contract defined in src-tauri/src/input/commands.rs
// ---------------------------------------------------------------------------

/** HID arm-state broadcast: mutation responses only reach the calling window,
 *  so every arm/disarm also emits the resulting HidArmedStatus app-wide — the
 *  overlay/tray affordance stays truthful when the settings window (or a future
 *  tray path) flips the arming toggle. Keep in sync with HID_STATE_EVENT in
 *  src-tauri/src/input/commands.rs (pinned by a Rust test). */
export const HID_STATE_EVENT = "hid://state";

/** Accessibility permission snapshot — health-as-value, never an error. The
 *  serde camelCase serialization of Rust's InputPermission. `supported: false`
 *  means this platform has no HID backend, so the arming affordance is inert. */
export interface InputPermission {
  granted: boolean;
  supported: boolean;
}

/** Kind-tagged HID failure taxonomy — the serde serialization of Rust's
 *  InputError (R007: every failure is typed, never a bare string). `disabled`
 *  is the structural-inertness refusal (D038); `permission-denied` is the kind
 *  the arming walkthrough keys on. */
export type InputError =
  | { kind: "disabled"; detail: string }
  | { kind: "permission-denied"; detail: string }
  | { kind: "unsupported"; platform: string; detail: string }
  | { kind: "input-failed"; detail: string };

/** The HID run mode (S04) — the kebab-case serde tags of Rust's HidRunMode.
 *  `off` is structurally inert (D038); `ask` prompts inline before each not-yet-
 *  whitelisted action kind; `auto-run` performs every action without prompting
 *  (the "dangerously allows all input" mode). `off` is the safe default a
 *  missing/garbage persisted value falls back to. */
export type HidRunMode = "off" | "ask" | "auto-run";

/** Queryable HID arming state `{ armed, mode, permission, error }` —
 *  health-as-value beside `privacy_status` (R007), the serde camelCase
 *  serialization of Rust's HidArmedStatus. `mode` is the three-way run mode the
 *  Settings selector reads; `armed` is `mode !== "off"`, kept for the S03
 *  boolean surface. `error` carries the most recent refused select (permission-
 *  denied) or persist failure (input-failed), typed so the walkthrough matches
 *  on `kind`. Null `error` means the last mutation succeeded. */
export interface HidArmedStatus {
  armed: boolean;
  mode: HidRunMode;
  permission: InputPermission;
  error: InputError | null;
}

/** Current HID arming state (health-as-value, like `llm_health`). Safe to poll
 *  while the walkthrough waits for the user to grant Accessibility. */
export function hidArmedStatus(): Promise<HidArmedStatus> {
  return invoke<HidArmedStatus>("hid_armed_status");
}

/** Arm or disarm HID. Never rejects backend-side — a refused arm (permission-
 *  denied) or persist failure rides `error` on the returned authoritative
 *  status, same contract as `set_privacy_mode`/`set_nudges_enabled`. */
export function setHidArmed(arm: boolean): Promise<HidArmedStatus> {
  return invoke<HidArmedStatus>("set_hid_armed", { arm });
}

/** Select the HID run mode (S04) — the three-way successor to `setHidArmed`.
 *  Never rejects backend-side: a refused select (permission-denied → walkthrough)
 *  or persist failure rides `error` on the returned authoritative status, same
 *  contract as `setHidArmed`/`set_privacy_mode`. */
export function setHidRunMode(mode: HidRunMode): Promise<HidArmedStatus> {
  return invoke<HidArmedStatus>("set_hid_run_mode", { mode });
}

/** Deep-link to System Settings → Privacy & Security → Accessibility — the
 *  arming walkthrough's "Open System Settings" action. Rejects with a typed
 *  InputError (`unsupported` off macOS). */
export function openInputSettings(): Promise<void> {
  return invoke("open_input_settings");
}

/** Subscribe to the app-wide HID arm-state broadcast (`hid://state`). */
export function onHidStateChanged(cb: (status: HidArmedStatus) => void): Promise<UnlistenFn> {
  return listen<HidArmedStatus>(HID_STATE_EVENT, (e) => cb(e.payload));
}

// ---------------------------------------------------------------------------
// HID per-action approval IPC (S04/M005) — contract defined in
// src-tauri/src/llm/commands.rs (event + payload) and the pure resolver in
// src-tauri/src/input/commands.rs
// ---------------------------------------------------------------------------

/** HID approval-request broadcast: the backend approval gate emits this when an
 *  Ask-mode action whose kind is not yet whitelisted needs the user's decision,
 *  and awaits a `respond_hid_approval` reply. Keep in sync with HID_APPROVAL_EVENT
 *  in src-tauri/src/llm/commands.rs (pinned by a Rust test and its TS twin). */
export const HID_APPROVAL_EVENT = "hid://approval-request";
/** Broadcast when a pending approval stops being pending (a verdict landed
 *  from ANY window, or the gate timed out): every surface showing the card
 *  removes it. Keep in sync with HID_APPROVAL_RESOLVED_EVENT in Rust. */
export const HID_APPROVAL_RESOLVED_EVENT = "hid://approval-resolved";

/** The kind of a HID action, stripped of payload — the granularity the session
 *  whitelist grants by. The kebab-case serde tags of Rust's ActionKind.
 *  "focus-app" is the HID-class `focus_app` tool (bring an app to the front),
 *  gated through the same approval path as the input actions (M005). */
export type ActionKind =
  | "mouse-move"
  | "mouse-click"
  | "mouse-drag"
  | "scroll"
  | "type-text"
  | "key-press"
  | "focus-app"
  | "clipboard"
  | "write-file"
  | "run-in-workspace"
  | "run-command";

/** The `hid://approval-request` payload — the serde camelCase serialization of
 *  Rust's ApprovalRequestPayload. Pixel-free: a correlation id, the action kind,
 *  and a human summary the overlay shows; never a screenshot or coordinate. */
export interface HidApprovalRequest {
  approvalId: number;
  kind: ActionKind;
  summary: string;
}

/** The user's answer to an approval prompt — the kebab-case wire strings Rust's
 *  ApprovalVerdict deserializes. "allow-once" performs this one action;
 *  "allow-kind" also whitelists the kind for the session; "deny" refuses. */
export type ApprovalVerdict = "allow-once" | "allow-kind" | "allow-always" | "deny";

/** The permanently approved action kinds (Settings list; kebab strings). */
export function approvedActionKinds(): Promise<string[]> {
  return invoke<string[]>("approved_action_kinds");
}

/** Withdraw a permanent grant; resolves with the remaining set. Also revokes
 *  it from the RUNNING session's whitelist, so the next action prompts. */
export function removeApprovedActionKind(kind: string): Promise<string[]> {
  return invoke<string[]>("remove_approved_action_kind", { kind });
}

/** Subscribe to the HID approval-request broadcast (`hid://approval-request`). */
export function onHidApprovalRequest(
  cb: (request: HidApprovalRequest) => void,
): Promise<UnlistenFn> {
  return listen<HidApprovalRequest>(HID_APPROVAL_EVENT, (e) => cb(e.payload));
}

/** Deliver the user's verdict for a pending HID approval. Resolves to whether a
 *  live waiter received it (`false` if the gate already timed out) — safe to
 *  fire-and-forget; the backend never rejects. */
export function respondHidApproval(
  approvalId: number,
  verdict: ApprovalVerdict,
): Promise<boolean> {
  return invoke<boolean>("respond_hid_approval", { approvalId, verdict });
}

/** The approval-resolved payload: just the correlation id. */
export interface ApprovalResolvedPayload {
  approvalId: number;
}

/** Subscribe to HID approval resolutions (answered anywhere, or timed out). */
export function onHidApprovalResolved(
  cb: (payload: ApprovalResolvedPayload) => void,
): Promise<UnlistenFn> {
  return listen<ApprovalResolvedPayload>(HID_APPROVAL_RESOLVED_EVENT, (e) => cb(e.payload));
}

// ---------------------------------------------------------------------------
// MCP per-tool approval IPC (S04/M007) — contract defined in
// src-tauri/src/llm/commands.rs (event + payload) and the pure resolver in
// src-tauri/src/llm/mcp.rs. The MCP twin of the HID approval contract above.
// ---------------------------------------------------------------------------

/** MCP approval-request broadcast: the backend MCP approval gate emits this when
 *  an Ask-mode external tool call whose namespaced name is not yet allowlisted
 *  needs the user's decision, and awaits a `respond_mcp_approval` reply. Keep in
 *  sync with MCP_APPROVAL_EVENT in src-tauri/src/llm/commands.rs (pinned by a Rust
 *  test and its TS twin). */
export const MCP_APPROVAL_EVENT = "mcp://approval-request";
export const MCP_APPROVAL_RESOLVED_EVENT = "mcp://approval-resolved";

/** The `mcp://approval-request` payload — the serde camelCase serialization of
 *  Rust's McpApprovalRequestPayload. Pixel-free: a correlation id, the namespaced
 *  tool name, and a bounded human summary (the tool name plus a short argument
 *  preview) the overlay shows; never a screenshot or the full arguments (R011). */
export interface McpApprovalRequest {
  approvalId: number;
  toolName: string;
  summary: string;
}

/** The user's answer to an MCP approval prompt — the kebab-case wire strings
 *  Rust's McpApprovalVerdict deserializes. Keyed on the tool NAME (not a fixed
 *  kind like the HID twin): "allow-once" performs this one call; "allow-tool"
 *  also allowlists that exact namespaced tool name for the session; "deny"
 *  refuses. A missing/garbage verdict is rejected backend-side (fail-closed). */
export type McpApprovalVerdict = "allow-once" | "allow-tool" | "deny";

/** Subscribe to the MCP approval-request broadcast (`mcp://approval-request`). */
export function onMcpApprovalRequest(
  cb: (request: McpApprovalRequest) => void,
): Promise<UnlistenFn> {
  return listen<McpApprovalRequest>(MCP_APPROVAL_EVENT, (e) => cb(e.payload));
}

/** Deliver the user's verdict for a pending MCP tool-call approval. Resolves to
 *  whether a live waiter received it (`false` if the gate already timed out) —
 *  safe to fire-and-forget; the backend never rejects. */
export function respondMcpApproval(
  approvalId: number,
  verdict: McpApprovalVerdict,
): Promise<boolean> {
  return invoke<boolean>("respond_mcp_approval", { approvalId, verdict });
}

/** Subscribe to MCP approval resolutions — the MCP twin. */
export function onMcpApprovalResolved(
  cb: (payload: ApprovalResolvedPayload) => void,
): Promise<UnlistenFn> {
  return listen<ApprovalResolvedPayload>(MCP_APPROVAL_RESOLVED_EVENT, (e) => cb(e.payload));
}

// ---------------------------------------------------------------------------
// Chat run-state + Stop control IPC (S04 T04) — contract defined in
// src-tauri/src/llm/commands.rs
// ---------------------------------------------------------------------------

/** Chat run-state broadcast: the backend emits the resulting RunPhase app-wide
 *  on every transition (running on chat start, stopped when Stop cuts a run
 *  short, idle on a natural finish/error), so the overlay's Stop control appears
 *  exactly while a run is active. Keep in sync with RUN_STATE_EVENT in
 *  src-tauri/src/llm/commands.rs (pinned by a Rust test and its TS twin). */
export const RUN_STATE_EVENT = "llm://run-state";

/** The coarse chat run-state — the kebab-case serde tags of Rust's RunPhase.
 *  "running" shows the Stop control; "idle"/"stopped" hide it. */
export type RunPhase = "idle" | "running" | "stopped";

/** The `llm://run-state` / `run_state` payload — the serde camelCase
 *  serialization of Rust's RunStatePayload. */
export interface RunStatePayload {
  phase: RunPhase;
}

/** Current chat run-state (health-as-value, like `llm_health`). The overlay
 *  queries this at mount to render the Stop control truthfully before any
 *  broadcast arrives. */
export function runState(): Promise<RunStatePayload> {
  return invoke<RunStatePayload>("run_state");
}

/** Stop the in-flight chat run: resolves to the resulting run-state. Never
 *  rejects backend-side — a Stop with nothing in flight returns `idle`, so it
 *  is safe to fire without racing the run's own completion. */
export function stopChat(): Promise<RunStatePayload> {
  return invoke<RunStatePayload>("stop_chat");
}

/** Subscribe to the app-wide chat run-state broadcast (`llm://run-state`). */
export function onRunState(cb: (payload: RunStatePayload) => void): Promise<UnlistenFn> {
  return listen<RunStatePayload>(RUN_STATE_EVENT, (e) => cb(e.payload));
}

// ---------------------------------------------------------------------------
// First-run onboarding IPC (M006) — contract defined in
// src-tauri/src/onboarding/mod.rs. The overlay shows a one-time explainer on
// first launch, then requests Screen Recording + Accessibility with context and
// marks onboarding done. Requesting Accessibility does NOT arm HID (D038/R019).
// ---------------------------------------------------------------------------

/** The first-run onboarding snapshot — the serde camelCase serialization of
 *  Rust's FirstRunStatus. `pending` is the signal to show the explainer;
 *  `capture`/`input` are the live permission states so the panel can reflect
 *  what is already granted; `persistError` carries a failed "mark done" save. */
export interface FirstRunStatus {
  pending: boolean;
  capture: CapturePermission;
  input: InputPermission;
  persistError: string | null;
}

/** Read the first-run onboarding snapshot (health-as-value; never rejects
 *  backend-side). Outside a Tauri runtime the invoke rejects and the caller
 *  absorbs it — the overlay simply skips onboarding. */
export function firstRunStatus(): Promise<FirstRunStatus> {
  return invoke<FirstRunStatus>("first_run_status");
}

/** Request the Screen Recording OS prompt during onboarding; resolves to the
 *  resulting permission. Spends the one-shot macOS TCC prompt — after a prior
 *  denial it comes back ungranted and the Settings deep-link is the recourse. */
export function requestCapturePermission(): Promise<CapturePermission> {
  return invoke<CapturePermission>("request_capture_permission");
}

/** Request the Accessibility OS prompt during onboarding; resolves to the
 *  resulting permission. Requesting the grant does NOT arm HID — arming stays the
 *  explicit Settings choice (D038/R019); this only pre-grants the OS permission. */
export function requestInputPermission(): Promise<InputPermission> {
  return invoke<InputPermission>("request_input_permission");
}

/** Mark first-run onboarding complete so the explainer never shows again —
 *  called whether the user finished the permission steps or skipped them. Never
 *  rejects backend-side; a persist failure rides `persistError` on the returned
 *  status, same contract as the other health-as-value mutations. */
export function completeFirstRun(): Promise<FirstRunStatus> {
  return invoke<FirstRunStatus>("complete_first_run");
}

/** Memory-retention snapshot — the serde camelCase serialization of Rust's
 *  MemoryRetentionStatus (memory/commands.rs). `retention` is always one of
 *  the wire values ("7d" | "30d" | "90d" | "forever"); `error` carries a
 *  rejected value or persist failure as data (never an IPC rejection). */
export interface MemoryRetentionStatus {
  retention: string;
  error: string | null;
}

/** Read the effective memory-retention setting (tour Memory step, Settings). */
export function memoryRetention(): Promise<MemoryRetentionStatus> {
  return invoke<MemoryRetentionStatus>("memory_retention");
}

/** Persist the memory-retention setting. Display/persist only this milestone —
 *  pruning that honors it is a specced follow-up. */
export function setMemoryRetention(retention: string): Promise<MemoryRetentionStatus> {
  return invoke<MemoryRetentionStatus>("set_memory_retention", { retention });
}

/** Global-hotkey registration snapshot — serde camelCase of Rust's
 *  HotkeyStatus (hotkey.rs): the live shortcut string (e.g.
 *  "super+shift+space"), whether it registered, and any conflict error. */
export interface HotkeyStatus {
  shortcut: string;
  registered: boolean;
  error: string | null;
}

/** Read the live hotkey binding (tour Summon step shows the real shortcut). */
export function hotkeyStatus(): Promise<HotkeyStatus> {
  return invoke<HotkeyStatus>("hotkey_status");
}

/** Terminal-commands gate snapshot (computer-control I2) — serde camelCase
 *  of Rust's CommandsStatus. Default OFF; `error` carries a persist failure
 *  as data (never an IPC rejection). */
export interface CommandsStatus {
  enabled: boolean;
  /** Persistent user-defined allowlist: entries matching a command (exact,
   *  or entry + space-separated tail) run without an approval prompt. */
  allowlist: string[];
  error: string | null;
}

/** Read the terminal-commands gate. */
export function commandsStatus(): Promise<CommandsStatus> {
  return invoke<CommandsStatus>("commands_status");
}

/** Flip the terminal-commands gate (Settings → Automation). A persist
 *  failure rolls back and returns as data. */
export function setCommandsEnabled(enable: boolean): Promise<CommandsStatus> {
  return invoke<CommandsStatus>("set_commands_enabled", { enable });
}

/** Replace the persistent command allowlist (Settings editor). Sanitized
 *  server-side; a persist failure rolls back and returns as data. */
export function setCommandsAllowlist(entries: string[]): Promise<CommandsStatus> {
  return invoke<CommandsStatus>("set_commands_allowlist", { entries });
}

/** Machine-inventory health (computer-control I1) — serde camelCase of
 *  Rust's InventoryStatus. */
export interface InventoryStatus {
  apps: number;
  tools: number;
  lastRefreshMs: number | null;
  error: string | null;
}

/** One cached program — serde camelCase of Rust's InventoryEntry. */
export interface InventoryEntry {
  name: string;
  path: string;
  kind: string;
  refreshedAtMs: number;
}

export function inventoryStatus(): Promise<InventoryStatus> {
  return invoke<InventoryStatus>("inventory_status");
}

export function inventorySearch(query: string, limit?: number): Promise<InventoryEntry[]> {
  return invoke<InventoryEntry[]>("inventory_search", { query, limit });
}

/** Re-scan the machine now; resolves with the resulting status. */
export function refreshInventory(): Promise<InventoryStatus> {
  return invoke<InventoryStatus>("refresh_inventory");
}

/** One stored chat session's list row (computer-control I3) — serde
 *  camelCase of Rust's ChatSessionSummary. `title` is the first user line. */
export interface ChatSessionSummary {
  id: number;
  startedAtMs: number;
  lastAtMs: number;
  title: string;
  messageCount: number;
}

/** One transcript line of a stored session. */
export interface ChatSessionMessage {
  role: string;
  text: string;
  atMs: number;
}

/** Start a fresh chat session; subsequent exchanges append to it. */
/** Resume a stored session: subsequent exchanges append to it; resolves
 *  with its ordered transcript for seeding the view. */
export function chatResumeSession(id: number): Promise<ChatSessionMessage[]> {
  return invoke<ChatSessionMessage[]>("chat_resume_session", { id });
}

/** Delete one stored session and its transcript (purge, 2026-07-27). */
export function chatSessionDelete(id: number): Promise<void> {
  return invoke<void>("chat_session_delete", { id });
}

/** Delete every stored session; resolves with how many went. */
export function chatSessionsWipe(): Promise<number> {
  return invoke<number>("chat_sessions_wipe");
}

export function chatNewSession(): Promise<number> {
  return invoke<number>("chat_new_session");
}

/** Newest-first stored session summaries (memory window Chats tab). A
 *  non-empty `query` searches across stored transcript text. */
export function chatSessions(limit?: number, query?: string): Promise<ChatSessionSummary[]> {
  return invoke<ChatSessionSummary[]>("chat_sessions", { limit, query });
}

/** One stored session's ordered transcript. */
export function chatSessionMessages(id: number): Promise<ChatSessionMessage[]> {
  return invoke<ChatSessionMessage[]>("chat_session_messages", { id });
}

/** Whether an LLM endpoint URL points at this machine — the truth condition
 *  for the palette's "● on-device" badge (no-fake-data rule: the badge only
 *  renders when the model actually runs locally). Unparseable URLs are not
 *  local: when we can't tell, we don't claim. */
export function isLocalEndpoint(endpoint: string): boolean {
  try {
    const host = new URL(endpoint).hostname;
    return host === "localhost" || host === "127.0.0.1" || host === "::1" || host === "[::1]";
  } catch {
    return false;
  }
}

// ---------------------------------------------------------------------------
// Chat state machine (pure)
// ---------------------------------------------------------------------------

export type AssistantStatus = "streaming" | "done" | "interrupted";

/** Memory-consulted lifecycle on an assistant answer (S03): "searching"
 *  while a requested memory_search executes, "consulted" once a successful
 *  result landed. A failed search clears back to undefined — the model still
 *  answers, but the answer is not memory-grounded and must not claim so. */
export type MemoryPhase = "searching" | "consulted";

/** One terminal command executed during an assistant turn (computer-control
 *  I2): rendered as the transcript's monospace terminal block. `ok: null`
 *  while running; `preview` is the bounded output from the result event. */
/** One tool step in the transcript's collapsible process block. */
export interface ChatStep {
  callId: string;
  label: string;
  ok: boolean | null;
}

export interface TerminalRun {
  callId: string;
  command: string;
  ok: boolean | null;
  preview: string | null;
}

/** One workspace_diff review during an assistant turn (coding-agent S5):
 *  rendered as a collapsible colored diff block. `ok: null` while running;
 *  `report` is the bounded status+diff text from the result event. */
export interface DiffBlock {
  callId: string;
  ok: boolean | null;
  report: string | null;
}

/** Colorization kind for one line of a diff report (pure, CSS-mapped). */
export function diffLineKind(line: string): "add" | "del" | "hunk" | "meta" | "context" {
  if (line.startsWith("+++") || line.startsWith("---") || line.startsWith("diff --git")) {
    return "meta";
  }
  if (line.startsWith("@@")) return "hunk";
  if (line.startsWith("+")) return "add";
  if (line.startsWith("-")) return "del";
  return "context";
}

export interface UiMessage {
  role: "user" | "assistant";
  text: string;
  /** Always "done" for user messages; lifecycle state for assistant ones. */
  status: AssistantStatus;
  /** True when a screen attachment rode this user turn (renders the chip). */
  attached?: boolean;
  /** Assistant only: terminal commands run during this turn (I2). */
  terminal?: TerminalRun[];
  /** Assistant only: workspace_diff reviews during this turn (S5). */
  diffs?: DiffBlock[];
  /** Assistant only: EVERY tool step of this turn (the HUD trail's
   *  transcript twin, 2026-08-01) — the process record survives after the
   *  pill dismisses. `ok: null` = still running. */
  steps?: ChatStep[];
  /** Assistant only: memory_search tool phase (renders the indicator). */
  memory?: MemoryPhase;
  /** Assistant only: real token spend for the turn (in/out), when the
   *  backend reported usage. */
  usage?: { promptTokens: number; completionTokens: number };
  /** Assistant only: accumulated chain-of-thought from a thinking model,
   *  rendered as a dimmed "Thinking…" region above the answer. Transient —
   *  streamed via llm://reasoning, never sent back as history or persisted.
   *  Undefined when the model streamed no reasoning. */
  reasoning?: string;
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
  /** Live background-wait status (loading model / reading prompt); null
   *  whenever anything is actually streaming. */
  phase: PhasePayload | null;
  /** Session token spend: summed real usage of every completed run. */
  sessionTokens: { promptTokens: number; completionTokens: number };
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
  /** The nudge currently parked on the idle overlay (drives the edge
   *  banner); null when none is showing. */
  nudge: NudgePayload | null;
  /** Context staged by a summon-from-nudge (`nudge://dismiss` reason
   *  "summoned"): grounds the next submit via a prepended system message,
   *  then clears — consume-once, like the screen attachment. */
  nudgePreload: NudgePayload | null;
  /** Coarse chat run-state (S04 T04) behind the overlay Stop control: "running"
   *  from submit until the backend's terminal run-state broadcast, then
   *  "idle"/"stopped". Fed by submit and the `llm://run-state` broadcast. */
  runPhase: RunPhase;
  /** Pending HID/run_command approval prompts (hid://approval-request): the
   *  gate parks the action until the user answers or the 120s backend
   *  timeout denies. Rendered as the overlay's approval block; answering
   *  fires respond_hid_approval and removes the entry. */
  hidApprovals: HidApprovalRequest[];
  /** Pending MCP tool-call approvals (mcp://approval-request) — the MCP twin. */
  mcpApprovals: McpApprovalRequest[];
}

export const initialChatState: ChatState = {
  messages: [],
  requestId: null,
  awaitingId: false,
  buffered: [],
  banner: null,
  lastQuestion: null,
  phase: null,
  sessionTokens: { promptTokens: 0, completionTokens: 0 },
  modelInfo: null,
  attachment: null,
  attachPending: false,
  captureError: null,
  capturePermission: null,
  privacy: null,
  nudge: null,
  nudgePreload: null,
  runPhase: "idle",
  hidApprovals: [],
  mcpApprovals: [],
};

export type LlmEvent =
  | { type: "token"; payload: TokenPayload }
  | { type: "reasoning"; payload: ReasoningPayload }
  | { type: "done"; payload: DonePayload }
  | { type: "error"; payload: ErrorPayload }
  | { type: "tool-call"; payload: ToolCallPayload }
  | { type: "tool-result"; payload: ToolResultPayload }
  | { type: "terminal-chunk"; payload: TerminalChunkPayload }
  | { type: "phase"; payload: PhasePayload };

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
  | { type: "nudge-shown"; payload: NudgePayload }
  | { type: "nudge-dismissed"; reason: NudgeDismissReason }
  // The user removed the nudge-context chip: the next question goes out
  // ungrounded, exactly as if the nudge had never been summoned.
  | { type: "nudge-preload-cleared" }
  | { type: "run-state"; phase: RunPhase }
  // The New-chat control (I3): clear the conversation, keep the environment
  // snapshots (routing, permissions, privacy, nudge) — they describe the
  // machine, not the conversation.
  | { type: "new-chat" }
  // Resume a stored session: seed the transcript from its saved messages
  // (all settled — nothing was streaming when it was stored).
  | { type: "resume-chat"; messages: { role: string; text: string }[] }
  // Approval prompts (gate ↔ overlay): a request parks an action until its
  // verdict; answered removes it (the IPC reply itself is fired by the view).
  | { type: "hid-approval"; request: HidApprovalRequest }
  | { type: "hid-approval-answered"; approvalId: number }
  | { type: "mcp-approval"; request: McpApprovalRequest }
  | { type: "mcp-approval-answered"; approvalId: number }
  | LlmEvent;

function withLastAssistant(
  messages: UiMessage[],
  update: (msg: UiMessage) => UiMessage,
): UiMessage[] {
  const idx = messages.length - 1;
  if (idx < 0 || messages[idx].role !== "assistant") return messages;
  return [...messages.slice(0, idx), update(messages[idx])];
}

/** A "searching" phase on a settling answer means the result never landed
 *  (stream died or ended mid-dispatch) — clear it rather than leave a live
 *  "searching memory" claim on a finished message. "consulted" survives. */
function settleMemoryPhase(memory: MemoryPhase | undefined): MemoryPhase | undefined {
  return memory === "searching" ? undefined : memory;
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
    m.status === "streaming"
      ? { ...m, status: "interrupted", memory: settleMemoryPhase(m.memory) }
      : m,
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

/** The prepended system message grounding a summon-from-nudge chat in the
 *  triggering screen context and the memories fetched at classification
 *  time (no new IPC on the hotkey path — everything rode `nudge://show`). */
export function nudgeContextMessage(
  payload: NudgePayload,
  hasScreenshot = false,
): ChatMessage {
  const parts = [
    `Third Eye just showed the user a proactive nudge: "${payload.message}". ` +
      "The user opened chat to follow up on it — ground your answers in the " +
      "screen context below (the screen may have changed since).",
    `Screen text at the time${payload.appContext ? ` (frontmost app: ${payload.appContext})` : ""}:\n${payload.screenText}`,
  ];
  if (payload.memoryContext.length > 0) {
    parts.push(
      `Relevant stored memories:\n${payload.memoryContext.map((m) => `- ${m}`).join("\n")}`,
    );
  }
  if (hasScreenshot) {
    parts.push(
      "A screenshot taken when the nudge appeared is attached to the user's " +
        "message — look at it for anything the text above misses.",
    );
  }
  return { role: "system", content: parts.join("\n\n") };
}

/** Build the wire history for a new question from completed turns only —
 *  interrupted partials and the streaming placeholder are excluded. Only the
 *  outgoing turn carries attachments: past screenshots are not resent as
 *  history (the answer they grounded already is), keeping requests small.
 *  A staged nudge preload prepends its system context message. */
export function composeMessages(
  messages: UiMessage[],
  question: string,
  attachments: Attachment[] = [],
  preload: NudgePayload | null = null,
  preloadScreenshot = false,
): ChatMessage[] {
  const history: ChatMessage[] = [];
  if (preload) history.push(nudgeContextMessage(preload, preloadScreenshot));
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
        m.status === "streaming"
          ? { ...m, status: "interrupted", memory: settleMemoryPhase(m.memory) }
          : m,
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
        phase: null,
        sessionTokens: state.sessionTokens,
        modelInfo: state.modelInfo,
        attachment: null,
        // Dropping the pending flag makes a capture that settles after this
        // submit stale — its frame is discarded, not silently re-staged.
        attachPending: false,
        captureError: null,
        capturePermission: state.capturePermission,
        privacy: state.privacy,
        nudge: state.nudge,
        // The preload grounds exactly this submit (App.tsx read it into the
        // composed history before dispatching) — consumed here so it can
        // never ride a later, unrelated question.
        nudgePreload: null,
        // Pending approvals belong to the (single-flight) run being replaced;
        // the backend aborts it, and any parked gate resolves by timeout.
        hidApprovals: state.hidApprovals,
        mcpApprovals: state.mcpApprovals,
        // A submit starts a run: show the Stop control immediately, before the
        // backend's `running` broadcast lands (which confirms the same state).
        runPhase: "running",
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
        // The chat invoke itself rejected — no backend run started, so no
        // run-state broadcast will arrive to clear the Stop control; do it here.
        runPhase: "idle",
      };
    case "token": {
      if (state.awaitingId) return { ...state, buffered: [...state.buffered, action] };
      if (action.payload.requestId !== state.requestId) return state; // stale
      return {
        ...state,
        phase: null,
        messages: withLastAssistant(state.messages, (m) => ({
          ...m,
          text: m.text + action.payload.token,
        })),
      };
    }
    case "reasoning": {
      // Same stale-filtering and pre-resolve buffering as tokens: a reasoning
      // delta from an aborted predecessor must not touch the active answer. It
      // appends to the transient `reasoning` region, never to `text`.
      if (state.awaitingId) return { ...state, buffered: [...state.buffered, action] };
      if (action.payload.requestId !== state.requestId) return state; // stale
      return {
        ...state,
        phase: null,
        messages: withLastAssistant(state.messages, (m) => ({
          ...m,
          reasoning: (m.reasoning ?? "") + action.payload.delta,
        })),
      };
    }
    case "new-chat":
      return {
        ...initialChatState,
        modelInfo: state.modelInfo,
        capturePermission: state.capturePermission,
        privacy: state.privacy,
        nudge: state.nudge,
        runPhase: state.runPhase,
        // A run (and its parked approvals) outlives the transcript reset —
        // dropping these would hang the gate until its timeout denies.
        hidApprovals: state.hidApprovals,
        mcpApprovals: state.mcpApprovals,
      };
    case "resume-chat":
      return {
        ...initialChatState,
        // Stored transcripts only hold user/assistant lines; anything else
        // (a future role) is dropped rather than rendered as a wrong bubble.
        messages: action.messages
          .filter((m) => m.role === "user" || m.role === "assistant")
          .map((m) => ({
            role: m.role as "user" | "assistant",
            text: m.text,
            status: "done" as const,
          })),
        modelInfo: state.modelInfo,
        capturePermission: state.capturePermission,
        privacy: state.privacy,
        nudge: state.nudge,
        runPhase: state.runPhase,
        hidApprovals: state.hidApprovals,
        mcpApprovals: state.mcpApprovals,
      };
    case "hid-approval":
      // Replay-safe: the same approvalId folds once.
      if (state.hidApprovals.some((r) => r.approvalId === action.request.approvalId)) return state;
      return { ...state, hidApprovals: [...state.hidApprovals, action.request] };
    case "hid-approval-answered":
      return {
        ...state,
        hidApprovals: state.hidApprovals.filter((r) => r.approvalId !== action.approvalId),
      };
    case "mcp-approval":
      if (state.mcpApprovals.some((r) => r.approvalId === action.request.approvalId)) return state;
      return { ...state, mcpApprovals: [...state.mcpApprovals, action.request] };
    case "mcp-approval-answered":
      return {
        ...state,
        mcpApprovals: state.mcpApprovals.filter((r) => r.approvalId !== action.approvalId),
      };
    case "tool-call": {
      // Same stale-filtering and pre-resolve buffering as tokens: a tool
      // phase from an aborted predecessor must not touch the active answer.
      if (state.awaitingId) return { ...state, buffered: [...state.buffered, action] };
      if (action.payload.requestId !== state.requestId) return state; // stale
      state = { ...state, phase: null };
      // Every call lands in the steps block — the durable process record
      // (the HUD trail dies with the pill; this survives in the transcript).
      const described = describeCall(action.payload.call.name, action.payload.call.arguments);
      const withStep = {
        ...state,
        messages: withLastAssistant(state.messages, (m) => ({
          ...m,
          steps: (m.steps ?? []).some((s) => s.callId === action.payload.call.id)
            ? m.steps
            : [
                ...(m.steps ?? []),
                { callId: action.payload.call.id, label: described.label, ok: null },
              ],
        })),
      };
      if (
        action.payload.call.name === RUN_COMMAND_TOOL ||
        action.payload.call.name === RUN_IN_WORKSPACE_TOOL
      ) {
        // The exact command line, straight from the call's own arguments —
        // the transcript block shows what actually runs (I2 visibility).
        let command = action.payload.call.arguments;
        try {
          const parsed: unknown = JSON.parse(action.payload.call.arguments);
          if (parsed && typeof parsed === "object" && typeof (parsed as { command?: unknown }).command === "string") {
            command = (parsed as { command: string }).command;
          }
        } catch {
          // Malformed args still execute-and-fail loop-side; show them raw.
        }
        const run: TerminalRun = {
          callId: action.payload.call.id,
          command,
          ok: null,
          preview: null,
        };
        return {
          ...withStep,
          messages: withLastAssistant(withStep.messages, (m) => ({
            ...m,
            terminal: [...(m.terminal ?? []), run],
          })),
        };
      }
      if (action.payload.call.name === WORKSPACE_DIFF_TOOL) {
        const block: DiffBlock = { callId: action.payload.call.id, ok: null, report: null };
        return {
          ...withStep,
          messages: withLastAssistant(withStep.messages, (m) => ({
            ...m,
            diffs: [...(m.diffs ?? []), block],
          })),
        };
      }
      if (action.payload.call.name !== MEMORY_SEARCH_TOOL) return withStep;
      return {
        ...withStep,
        messages: withLastAssistant(withStep.messages, (m) => ({ ...m, memory: "searching" })),
      };
    }
    case "tool-result": {
      if (state.awaitingId) return { ...state, buffered: [...state.buffered, action] };
      if (action.payload.requestId !== state.requestId) return state; // stale
      const settled = {
        ...state,
        messages: withLastAssistant(state.messages, (m) => ({
          ...m,
          steps: (m.steps ?? []).map((s) =>
            s.callId === action.payload.callId ? { ...s, ok: action.payload.ok } : s,
          ),
        })),
      };
      if (
        action.payload.name === RUN_COMMAND_TOOL ||
        action.payload.name === RUN_IN_WORKSPACE_TOOL
      ) {
        const { callId, ok, preview, failure } = action.payload;
        return {
          ...settled,
          messages: withLastAssistant(settled.messages, (m) => ({
            ...m,
            terminal: (m.terminal ?? []).map((run) =>
              run.callId === callId
                ? { ...run, ok, preview: preview ?? (failure ? `[${failure}]` : null) }
                : run,
            ),
          })),
        };
      }
      if (action.payload.name === WORKSPACE_DIFF_TOOL) {
        const { callId, ok, preview, failure } = action.payload;
        return {
          ...settled,
          messages: withLastAssistant(settled.messages, (m) => ({
            ...m,
            diffs: (m.diffs ?? []).map((block) =>
              block.callId === callId
                ? { ...block, ok, report: preview ?? (failure ? `[${failure}]` : null) }
                : block,
            ),
          })),
        };
      }
      if (action.payload.name !== MEMORY_SEARCH_TOOL) return settled;
      return {
        ...settled,
        messages: withLastAssistant(settled.messages, (m) => ({
          ...m,
          // A failed search clears "searching" without claiming consultation
          // (the model still answers, ungrounded). It never downgrades a
          // "consulted" earned by an earlier successful round.
          memory: action.payload.ok ? "consulted" : settleMemoryPhase(m.memory),
        })),
      };
    }
    case "phase": {
      // A wait ping only means anything for the CURRENT request, and only
      // while nothing is streaming (activity clears it below).
      if (state.awaitingId) return { ...state, buffered: [...state.buffered, action] };
      if (action.payload.requestId !== state.requestId) return state; // stale
      return { ...state, phase: action.payload };
    }
    case "terminal-chunk": {
      // Live build output (coding-agent S4): append to the RUNNING block's
      // preview, keeping the tail (a build's latest lines matter most). The
      // result event's bounded report replaces it when the command settles.
      if (state.awaitingId) return { ...state, buffered: [...state.buffered, action] };
      if (action.payload.requestId !== state.requestId) return state; // stale
      const { callId, chunk } = action.payload;
      return {
        ...state,
        messages: withLastAssistant(state.messages, (m) => ({
          ...m,
          terminal: (m.terminal ?? []).map((run) =>
            run.callId === callId && run.ok === null
              ? { ...run, preview: ((run.preview ?? "") + chunk).slice(-16384) }
              : run,
          ),
        })),
      };
    }
    case "done": {
      if (state.awaitingId) return { ...state, buffered: [...state.buffered, action] };
      if (action.payload.requestId !== state.requestId) return state;
      const usage =
        typeof action.payload.promptTokens === "number" &&
        typeof action.payload.completionTokens === "number"
          ? {
              promptTokens: action.payload.promptTokens,
              completionTokens: action.payload.completionTokens,
            }
          : undefined;
      return {
        ...state,
        phase: null,
        requestId: null,
        sessionTokens: usage
          ? {
              promptTokens: state.sessionTokens.promptTokens + usage.promptTokens,
              completionTokens:
                state.sessionTokens.completionTokens + usage.completionTokens,
            }
          : state.sessionTokens,
        messages: withLastAssistant(state.messages, (m) => ({
          ...m,
          // The backend's accumulated text is authoritative — it replaces any
          // frame-coalesced tail still buffered UI-side.
          text: action.payload.text,
          status: "done",
          memory: settleMemoryPhase(m.memory),
          usage,
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
              memory: settleMemoryPhase(m.memory),
            }))
          : settleStreamingTail(state.messages);
      return { ...state, requestId: null, phase: null, messages, banner: { error, online: false } };
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
    case "nudge-shown":
      // A new nudge supersedes any stale unconsumed preload: the context the
      // next summon should ground in is this one's.
      return { ...state, nudge: action.payload, nudgePreload: null };
    case "nudge-dismissed": {
      // The backend only emits nudge://dismiss when a nudge was actually
      // cleared, but guard anyway — a dismiss with nothing showing is a no-op
      // so it can't wipe a preload staged by an earlier summon.
      if (state.nudge === null) return state;
      return {
        ...state,
        nudge: null,
        // Every dismissal except a disable stages the context: the user
        // often summons chat AFTER the 12s auto-timeout took the banner
        // down, and "what was that nudge about" must still be answerable.
        // Freshness is enforced at submit time (freshNudgePreload), so a
        // stale stage never grounds an unrelated later chat. "disabled"
        // clears — the user turned the feature off.
        nudgePreload: action.reason === "disabled" ? null : state.nudge,
      };
    }
    case "nudge-preload-cleared":
      return { ...state, nudgePreload: null };
    case "run-state":
      // The backend's broadcast is authoritative: "running" when a run is live,
      // "idle"/"stopped" once it ends. Drives the Stop control's visibility.
      return { ...state, runPhase: action.phase };
  }
}

/** Whether the overlay Stop control should render (S04 T04): only while a run is
 *  active. A stopped or idle run hides it. Pure so the visibility is unit-tested
 *  without a DOM. */
export function showStopButton(state: ChatState): boolean {
  return state.runPhase === "running";
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
    case "tools-unsupported":
      return "This model can't use tools";
    case "interrupted":
      return "Answer interrupted";
    case "guard-blocked":
      return "Blocked by privacy guard";
    case "empty-completion":
      return "The model returned nothing";
    case "ipc":
      return "Chat unavailable";
  }
}

/** Detail line naming the endpoint that was tried (R006). "guard-blocked"
 *  carries a kebab-case reason instead of free-text detail — surface it
 *  verbatim so the machine-readable vocabulary stays greppable. */
export function bannerDetail(error: BannerError): string {
  if (error.kind === "ipc") return error.detail;
  if (error.kind === "guard-blocked") return `${error.endpoint} — ${error.reason}`;
  return `${error.endpoint} — ${error.detail}`;
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
