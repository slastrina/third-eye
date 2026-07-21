// UI side of the external-MCP-server IPC surface (S04/M007): the run-mode
// selector (`set_mcp_run_mode` / `mcp_status`), the `mcp://state` health
// broadcast, the persisted server list (`mcp_servers` / `set_mcp_servers`), and
// the pure `mcpReducer` behind the MCP Servers section in Settings. The shapes
// here mirror the serde camelCase / kebab-case serialization of Rust's
// McpHealthStatus, McpPhase, McpRunMode, McpServerConfig, and McpServersStatus
// (src-tauri/src/llm/mcp.rs + commands.rs) — a change on either side is a
// breaking IPC change, pinned by the Rust const/serde tests and their TS twins.
//
// The reducer is pure, so every health/server transition is unit-testable
// without a Tauri runtime (src/mcp-state.test.ts); Settings.tsx is only glue.
// (Kebab-case name per MEM051, matching cloud-state.ts/watcher-state.ts.)

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

/** MCP host health broadcast: every lifecycle transition (spawn → ready →
 *  crashed) and every run-mode mutation emits the resulting McpHealthStatus
 *  app-wide, so the Settings surface stays truthful whichever window changed the
 *  mode. Pinned Rust-side by `mcp_state_event_name_is_the_ipc_contract`. */
export const MCP_STATE_EVENT = "mcp://state";

/** Run mode for external MCP tool actions — the kebab-case serde tags of Rust's
 *  McpRunMode, mirroring HidRunMode. `off` is structurally inert (every external
 *  tool call refused before the wire — the fail-closed default a missing/garbage
 *  persisted value maps to); `ask` prompts before each not-yet-allowlisted tool
 *  name; `auto-run` runs every tool call without prompting. */
export type McpRunMode = "off" | "ask" | "auto-run";

/** The MCP child lifecycle phase — the kebab-case serde tags of Rust's McpPhase.
 *  `disconnected` = nothing enabled/spawned; `spawning` = launching + handshaking;
 *  `ready` = handshake done, tools reachable; `crashed` = a spawn/handshake
 *  failure or a mid-session drop (tools unavailable, the app keeps running —
 *  `lastError` names the cause). */
export type McpPhase = "disconnected" | "spawning" | "ready" | "crashed";

/** Queryable MCP host health (health-as-value, R007): returned by `mcp_status` /
 *  `set_mcp_run_mode`, broadcast on `mcp://state`. The serde camelCase shape of
 *  Rust's McpHealthStatus. `lastError` carries the most recent lifecycle failure
 *  so a crashed child stays diagnosable; `updatedAt` is epoch-millis of the last
 *  transition (`0` = none yet); `mode` mirrors the gate's live run mode;
 *  `toolCount` is how many tools the server advertised (`0` unless `ready`). */
export interface McpHealthStatus {
  phase: McpPhase;
  lastError: string | null;
  updatedAt: number;
  mode: McpRunMode;
  toolCount: number;
}

/** Which transport reaches one configured MCP server — the S05 discriminator,
 *  the kebab-case serde tags of Rust's McpTransport. `stdio` spawns a local child
 *  process (`command` + `args`); `http` connects to a remote streamable-HTTP / SSE
 *  endpoint at `url` with an optional keychain bearer token named by `authRef`.
 *  `stdio` is the default an S04 entry with no `transport` key falls back to. */
export type McpTransport = "stdio" | "http";

/** One user-configured external MCP server — the serde camelCase shape of Rust's
 *  McpServerConfig, the exact JSON persisted under `mcpServers` in settings.json.
 *  `id` is a stable key / display name; `enabled` (default false, fail-closed)
 *  gates whether it is spawned/connected. A `stdio` server (S04) uses `command` +
 *  `args` as the process the startup launch task spawns; an `http` server (S05)
 *  uses `url` plus an optional `authRef` naming the keychain bearer token (the
 *  secret itself never rides this shape — R018). `transport` always rides the wire
 *  (Rust serializes its default), so it is required here; `url`/`authRef` are
 *  omitted for a stdio entry (Rust `skip_serializing_if = "Option::is_none"`). */
export interface McpServerConfig {
  id: string;
  command: string;
  args: string[];
  enabled: boolean;
  transport: McpTransport;
  url?: string;
  authRef?: string;
}

/** Every transport, in UI order, with its human label — drives the add-server
 *  transport picker so a transport is added in one place. Mirrors
 *  MCP_RUN_MODE_OPTIONS. */
export const MCP_TRANSPORT_OPTIONS: readonly {
  readonly value: McpTransport;
  readonly label: string;
}[] = [
  { value: "stdio", label: "Local (stdio) — command" },
  { value: "http", label: "Remote (HTTP/SSE) — URL" },
];

/** The keychain account key convention the Settings UI files a remote server's
 *  bearer token under (the non-secret `authRef` persisted in settings.json). The
 *  secret bytes live in the OS keychain under this account; only this reference
 *  ever rides the config. Pure so the convention is unit-testable. */
export function mcpAuthRef(id: string): string {
  return `mcp:${id}`;
}

/** Queryable MCP server-list state (health-as-value): the serde camelCase shape
 *  of Rust's McpServersStatus. `servers` is always the authoritative persisted
 *  list (on a save failure it stays the last-persisted list); `persistError`
 *  carries the most recent `set_mcp_servers` failure so a change that could not
 *  be written stays visible (never an IPC rejection). */
export interface McpServersStatus {
  servers: McpServerConfig[];
  persistError: string | null;
}

/** Every run mode, in UI order, with its human label — drives the selector so a
 *  mode is added in one place. Mirrors HID_RUN_MODE_OPTIONS. */
export const MCP_RUN_MODE_OPTIONS: readonly { readonly value: McpRunMode; readonly label: string }[] =
  [
    { value: "off", label: "Off — no external tools (default)" },
    { value: "ask", label: "Ask — approve each tool" },
    { value: "auto-run", label: "Auto-run — no prompts" },
  ];

// ---------------------------------------------------------------------------
// Invoke wrappers
// ---------------------------------------------------------------------------

/** Current MCP host health — health-as-value beside `cloud_optin_status` /
 *  `watcher_status` (R007): a value at any time, never an error. The Settings
 *  MCP surface queries it at mount before any `mcp://state` broadcast arrives. */
export function mcpStatus(): Promise<McpHealthStatus> {
  return invoke<McpHealthStatus>("mcp_status");
}

/** Select the MCP run mode. Never rejects backend-side: a persist failure rides
 *  `lastError` on the returned authoritative status (rolled back and logged),
 *  the same health-as-value contract as `set_hid_run_mode` / `set_cloud_optin`. */
export function setMcpRunMode(mode: McpRunMode): Promise<McpHealthStatus> {
  return invoke<McpHealthStatus>("set_mcp_run_mode", { mode });
}

/** Current persisted MCP server list — health-as-value, never an error. */
export function mcpServers(): Promise<McpServersStatus> {
  return invoke<McpServersStatus>("mcp_servers");
}

/** Persist the MCP server list (add/remove). Never rejects backend-side: on a
 *  persist failure the returned status keeps the still-authoritative previous
 *  list and rides `persistError`, so an unpersisted change never appears saved.
 *  The change takes effect at the next startup launch task. */
export function setMcpServers(servers: McpServerConfig[]): Promise<McpServersStatus> {
  return invoke<McpServersStatus>("set_mcp_servers", { servers });
}

/** Subscribe to the app-wide MCP health broadcast (`mcp://state`). */
export function onMcpStateChanged(cb: (status: McpHealthStatus) => void): Promise<UnlistenFn> {
  return listen<McpHealthStatus>(MCP_STATE_EVENT, (e) => cb(e.payload));
}

// ---------------------------------------------------------------------------
// Remote-server bearer-token keychain IPC (S05) — the MCP twin of the cloud
// key surface (cloud-state.ts). Token material is write-only across this
// module: `setMcpAuth` carries a token inbound (the one legitimate crossing to
// the OS keychain) and NOTHING here ever returns or stores a token — only
// presence booleans cross IPC (R018), mirroring `set_cloud_api_key`. The shapes
// mirror the serde serialization of Rust's McpAuthStatus / McpAuthError
// (src-tauri/src/llm/commands.rs + mcp_keystore.rs).
// ---------------------------------------------------------------------------

/** Presence-only snapshot for one MCP auth account — the entire outbound
 *  vocabulary of the MCP keystore, the serde camelCase shape of Rust's
 *  McpAuthStatus. A single boolean: no field here ever carries the token (pinned
 *  Rust-side by `mcp_auth_status_carries_presence_boolean_only`). */
export interface McpAuthStatus {
  present: boolean;
}

/** A typed MCP keystore failure — the serde kind-tagged (kebab-case) / camelCase
 *  serialization of Rust's McpAuthError. `detail` never contains token material.
 *  `invalid-ref` = blank account key; `invalid-token` = blank token; both refused
 *  before the OS store is touched. `store-failed` = the keychain itself failed. */
export type McpAuthError =
  | { kind: "invalid-ref"; detail: string }
  | { kind: "invalid-token"; detail: string }
  | { kind: "store-failed"; detail: string };

/** Narrow an invoke rejection to the kind-tagged McpAuthError contract. Outside a
 *  Tauri runtime invoke rejects with a plain string/Error — that falls through
 *  here so the caller treats it as the no-runtime "unavailable" case, not a store
 *  failure (mirrors `isCloudKeyError`). */
export function isMcpAuthError(e: unknown): e is McpAuthError {
  if (typeof e !== "object" || e === null) return false;
  const kind = (e as { kind?: unknown }).kind;
  return kind === "invalid-ref" || kind === "invalid-token" || kind === "store-failed";
}

/** Store a remote MCP server's bearer token under its `authRef` account — the one
 *  legitimate inbound crossing of token material. Returns the fresh presence
 *  snapshot; NEVER returns the token. Rejects with an McpAuthError on a blank
 *  token / account or a store failure. */
export function setMcpAuth(authRef: string, token: string): Promise<McpAuthStatus> {
  return invoke<McpAuthStatus>("set_mcp_auth", { authRef, token });
}

/** Delete a remote MCP server's stored bearer token (deleting an absent token
 *  succeeds). Returns the fresh presence snapshot (`present: false` on success). */
export function deleteMcpAuth(authRef: string): Promise<McpAuthStatus> {
  return invoke<McpAuthStatus>("delete_mcp_auth", { authRef });
}

/** Presence snapshot for one `authRef` account — the write-only token field
 *  renders its "stored / not stored" state from this. Presence only, ever: the
 *  token itself has no outbound command (R018). Rejects with an McpAuthError when
 *  the OS store fails, or a plain string/Error outside a Tauri runtime. */
export function mcpAuthStatus(authRef: string): Promise<McpAuthStatus> {
  return invoke<McpAuthStatus>("mcp_auth_status", { authRef });
}

// ---------------------------------------------------------------------------
// MCP Settings state machine (pure)
// ---------------------------------------------------------------------------

export interface McpViewState {
  /** Live host health; null until the mount-time `mcp_status` resolves (or
   *  forever, outside Tauri — the section renders unavailable). */
  health: McpHealthStatus | null;
  /** The persisted server list; null until `mcp_servers` resolves. */
  servers: McpServerConfig[] | null;
  /** Most recent server-list persist failure, kept until the next server status
   *  supersedes it. Names what the save could not do without lying about what is
   *  currently persisted. */
  persistError: string | null;
}

export const initialMcpViewState: McpViewState = {
  health: null,
  servers: null,
  persistError: null,
};

export type McpViewAction =
  | { type: "health"; status: McpHealthStatus }
  | { type: "servers"; status: McpServersStatus };

export function mcpReducer(state: McpViewState, action: McpViewAction): McpViewState {
  switch (action.type) {
    case "health":
      // Mount-time query, set_mcp_run_mode responses, and the mcp://state
      // broadcast (any window's mode change or a lifecycle transition) all land
      // here — backend authoritative.
      return { ...state, health: action.status };
    case "servers":
      // A fresh server status is authoritative: its list replaces ours and its
      // persistError (null on success) supersedes any stale one.
      return { ...state, servers: action.status.servers, persistError: action.status.persistError };
  }
}

// ---------------------------------------------------------------------------
// Copy + selector helpers (pure)
// ---------------------------------------------------------------------------

/** Whether the selected mode warrants the "runs every external tool without a
 *  prompt" warning: only `auto-run`. Off and Ask never show it. Pure so the
 *  warning is unit-testable without a render. Mirrors hidModeShowsAutoRunWarning. */
export function mcpModeShowsAutoRunWarning(mode: McpRunMode): boolean {
  return mode === "auto-run";
}

/** A short human health line for the MCP section — phase plus, when crashed, the
 *  cause, and when ready, the advertised tool count. Pure so it is unit-testable. */
export function mcpHealthLine(health: McpHealthStatus): string {
  switch (health.phase) {
    case "disconnected":
      return "No external server running";
    case "spawning":
      return "Starting external server…";
    case "ready":
      return `Ready — ${health.toolCount} tool${health.toolCount === 1 ? "" : "s"} available`;
    case "crashed":
      return health.lastError
        ? `Tools unavailable — ${health.lastError}`
        : "Tools unavailable";
  }
}
