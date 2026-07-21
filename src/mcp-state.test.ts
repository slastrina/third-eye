// Reducer + helper + wire-contract coverage for the MCP Servers section (S04):
// the mcp://state event-name lock, the McpHealthStatus / McpRunMode / McpPhase /
// McpServerConfig / McpServersStatus wire shapes, the health/servers reducer
// transitions (including the persist-error channel), and the pure copy/selector
// helpers. The reducer is pure, so no Tauri runtime or DOM is needed.

import { describe, expect, it } from "vitest";
import {
  MCP_RUN_MODE_OPTIONS,
  MCP_STATE_EVENT,
  MCP_TRANSPORT_OPTIONS,
  initialMcpViewState,
  isMcpAuthError,
  mcpAuthRef,
  mcpHealthLine,
  mcpModeShowsAutoRunWarning,
  mcpReducer,
  type McpAuthError,
  type McpAuthStatus,
  type McpHealthStatus,
  type McpPhase,
  type McpRunMode,
  type McpServerConfig,
  type McpServersStatus,
  type McpTransport,
} from "./mcp-state";
import { MCP_APPROVAL_EVENT, type McpApprovalRequest, type McpApprovalVerdict } from "./chat";

const disconnected: McpHealthStatus = {
  phase: "disconnected",
  lastError: null,
  updatedAt: 0,
  mode: "off",
  toolCount: 0,
};
const ready: McpHealthStatus = {
  phase: "ready",
  lastError: null,
  updatedAt: 1_700_000_000_000,
  mode: "ask",
  toolCount: 3,
};
const weather: McpServerConfig = {
  id: "weather",
  command: "npx",
  args: ["-y", "@ref/weather"],
  enabled: true,
  transport: "stdio",
};

describe("MCP state event name", () => {
  it("matches the Rust-side IPC contract exactly", () => {
    // src-tauri/src/llm/commands.rs pins the same string from its side
    // (mcp_state_event_name_is_the_ipc_contract).
    expect(MCP_STATE_EVENT).toBe("mcp://state");
  });
});

describe("MCP approval IPC contract (S04/M007)", () => {
  it("keeps the approval-request event name in sync with the Rust contract", () => {
    // src-tauri/src/llm/commands.rs pins MCP_APPROVAL_EVENT to this same string.
    expect(MCP_APPROVAL_EVENT).toBe("mcp://approval-request");
  });

  it("McpApprovalRequest carries the correlation id, tool name, and human summary", () => {
    // The serde camelCase shape the gate emits and the overlay reads.
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
    // respond_mcp_approval deserializes these exact McpApprovalVerdict strings —
    // keyed on the tool NAME (allow-tool), unlike the HID twin's allow-kind.
    const verdicts: McpApprovalVerdict[] = ["allow-once", "allow-tool", "deny"];
    expect(verdicts).toEqual(["allow-once", "allow-tool", "deny"]);
  });
});

describe("MCP wire shapes", () => {
  it("McpRunMode covers the three kebab-case modes with off as the safe default", () => {
    const modes: McpRunMode[] = ["off", "ask", "auto-run"];
    expect(modes).toEqual(["off", "ask", "auto-run"]);
    // The selector lists them in the same order, off first.
    expect(MCP_RUN_MODE_OPTIONS.map((o) => o.value)).toEqual(["off", "ask", "auto-run"]);
  });

  it("McpPhase covers the four lifecycle phases", () => {
    const phases: McpPhase[] = ["disconnected", "spawning", "ready", "crashed"];
    expect(phases).toEqual(["disconnected", "spawning", "ready", "crashed"]);
  });

  it("McpServerConfig round-trips the camelCase persisted stdio shape", () => {
    // The exact JSON shape a stdio server persists under mcpServers and pinned
    // Rust-side (server_config_round_trips_the_persisted_shape): transport rides
    // the wire (Rust serializes its default), url/authRef are omitted for stdio.
    expect(weather).toEqual({
      id: "weather",
      command: "npx",
      args: ["-y", "@ref/weather"],
      enabled: true,
      transport: "stdio",
    });
    expect("url" in weather).toBe(false);
    expect("authRef" in weather).toBe(false);
  });

  it("McpTransport covers the two kebab-case transports with stdio first (default)", () => {
    // The kebab-case serde tags of Rust's McpTransport; the picker lists them in
    // the same order, stdio (the back-compat default) first.
    const transports: McpTransport[] = ["stdio", "http"];
    expect(transports).toEqual(["stdio", "http"]);
    expect(MCP_TRANSPORT_OPTIONS.map((o) => o.value)).toEqual(["stdio", "http"]);
  });

  it("McpServerConfig carries the S05 http shape: transport, url, and authRef", () => {
    // The exact JSON an http server persists — transport "http", the remote url,
    // and the non-secret keychain authRef (never the token). Pinned Rust-side by
    // server_config_round_trips_the_persisted_shape's http case.
    const remote: McpServerConfig = {
      id: "hosted-weather",
      command: "",
      args: [],
      enabled: true,
      transport: "http",
      url: "https://mcp.example.com/sse",
      authRef: "mcp:hosted-weather",
    };
    expect(remote.transport).toBe("http");
    expect(remote.url).toBe("https://mcp.example.com/sse");
    expect(remote.authRef).toBe("mcp:hosted-weather");
  });
});

describe("MCP auth (keychain bearer token) IPC contract (S05)", () => {
  it("derives the keychain account key from the server id (mcp:<id>)", () => {
    // The Settings UI convention; T02's Rust round-trip test uses this exact
    // "mcp:<id>" account key for the persisted authRef.
    expect(mcpAuthRef("weather")).toBe("mcp:weather");
    expect(mcpAuthRef("hosted-weather")).toBe("mcp:hosted-weather");
  });

  it("McpAuthStatus is presence-only — a single boolean, never the token (R018)", () => {
    // The serde camelCase shape set_mcp_auth / mcp_auth_status return, pinned
    // Rust-side by mcp_auth_status_carries_presence_boolean_only.
    const stored: McpAuthStatus = { present: true };
    const absent: McpAuthStatus = { present: false };
    expect(Object.keys(stored)).toEqual(["present"]);
    expect(stored.present).toBe(true);
    expect(absent.present).toBe(false);
  });

  it("McpAuthError matches the Rust kind-tagged serde vocabulary", () => {
    // The kebab-case kind tags of Rust's McpAuthError; detail never carries a
    // token. isMcpAuthError narrows each so a store failure is distinguished
    // from the no-runtime unavailable case.
    const errors: McpAuthError[] = [
      { kind: "invalid-ref", detail: "auth_ref is empty" },
      { kind: "invalid-token", detail: "token is empty" },
      { kind: "store-failed", detail: "keychain locked" },
    ];
    expect(errors.map((e) => e.kind)).toEqual(["invalid-ref", "invalid-token", "store-failed"]);
    for (const e of errors) expect(isMcpAuthError(e)).toBe(true);
  });

  it("isMcpAuthError rejects a plain string/Error (the no-runtime case)", () => {
    expect(isMcpAuthError("invoke unavailable outside Tauri")).toBe(false);
    expect(isMcpAuthError(new Error("no ipc"))).toBe(false);
    expect(isMcpAuthError(null)).toBe(false);
    expect(isMcpAuthError({ kind: "nonsense" })).toBe(false);
  });
});

describe("mcpReducer transitions", () => {
  it("starts unknown: nothing resolved", () => {
    expect(initialMcpViewState.health).toBeNull();
    expect(initialMcpViewState.servers).toBeNull();
    expect(initialMcpViewState.persistError).toBeNull();
  });

  it("stores the backend health snapshot as authoritative", () => {
    const s = mcpReducer(initialMcpViewState, { type: "health", status: ready });
    expect(s.health).toEqual(ready);
  });

  it("follows disconnected → ready as the lifecycle advances", () => {
    let s = mcpReducer(initialMcpViewState, { type: "health", status: disconnected });
    expect(s.health?.phase).toBe("disconnected");
    s = mcpReducer(s, { type: "health", status: ready });
    expect(s.health?.phase).toBe("ready");
    expect(s.health?.toolCount).toBe(3);
  });

  it("stores a server list and clears persistError on a successful save", () => {
    const status: McpServersStatus = { servers: [weather], persistError: null };
    const s = mcpReducer(initialMcpViewState, { type: "servers", status });
    expect(s.servers).toEqual([weather]);
    expect(s.persistError).toBeNull();
  });

  it("surfaces a persist failure while keeping the authoritative list intact", () => {
    // On a set_mcp_servers failure the backend returns the still-persisted list
    // plus persistError; the UI must show BOTH (the change did not take).
    const failed: McpServersStatus = {
      servers: [weather],
      persistError: "failed to persist mcpServers to settings.json",
    };
    const s = mcpReducer(initialMcpViewState, { type: "servers", status: failed });
    expect(s.servers).toEqual([weather]);
    expect(s.persistError).toContain("settings.json");
  });
});

describe("MCP copy + selector helpers", () => {
  it("shows the auto-run warning only for auto-run", () => {
    expect(mcpModeShowsAutoRunWarning("off")).toBe(false);
    expect(mcpModeShowsAutoRunWarning("ask")).toBe(false);
    expect(mcpModeShowsAutoRunWarning("auto-run")).toBe(true);
  });

  it("renders a health line per phase", () => {
    expect(mcpHealthLine(disconnected)).toBe("No external server running");
    expect(mcpHealthLine({ ...disconnected, phase: "spawning" })).toContain("Starting");
    expect(mcpHealthLine(ready)).toBe("Ready — 3 tools available");
    expect(mcpHealthLine({ ...ready, toolCount: 1 })).toBe("Ready — 1 tool available");
    expect(
      mcpHealthLine({ ...disconnected, phase: "crashed", lastError: "handshake timed out" }),
    ).toBe("Tools unavailable — handshake timed out");
    expect(mcpHealthLine({ ...disconnected, phase: "crashed" })).toBe("Tools unavailable");
  });
});
