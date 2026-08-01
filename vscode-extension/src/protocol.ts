// Bridge protocol v1 — the pure half of the extension (no vscode API, no
// sockets), unit-tested with node:test. Mirrors src-tauri/src/bridge/
// protocol.rs: the extension REFUSES a hello whose version it does not
// speak, parses only the known message set, and drops everything else.

export const BRIDGE_PROTOCOL_VERSION = 1;

/** The discovery file Third Eye writes at boot: where to connect + the
 *  per-boot token. */
export interface Discovery {
  port: number;
  token: string;
  version: number;
}

/** Third Eye's Tauri identifier — the app-data folder name. */
export const APP_IDENTIFIER = "com.slastrina.thirdeye";

/** Candidate absolute paths for bridge.json on this machine, most likely
 *  first. Pure: platform/home/env come in as arguments. */
export function discoveryCandidates(
  platform: NodeJS.Platform,
  home: string,
  env: Record<string, string | undefined>,
): string[] {
  switch (platform) {
    case "darwin":
      return [`${home}/Library/Application Support/${APP_IDENTIFIER}/bridge.json`];
    case "win32": {
      const appData = env.APPDATA ?? `${home}\\AppData\\Roaming`;
      return [`${appData}\\${APP_IDENTIFIER}\\bridge.json`];
    }
    default: {
      const xdg = env.XDG_DATA_HOME ?? `${home}/.local/share`;
      return [`${xdg}/${APP_IDENTIFIER}/bridge.json`];
    }
  }
}

/** Parse bridge.json content; null on anything malformed or an unknown
 *  protocol version (never connect with a contract we don't speak). */
export function parseDiscovery(raw: string): Discovery | null {
  try {
    const value: unknown = JSON.parse(raw);
    if (typeof value !== "object" || value === null) return null;
    const { port, token, version } = value as Record<string, unknown>;
    if (typeof port !== "number" || port <= 0 || port > 65535) return null;
    if (typeof token !== "string" || token.length === 0) return null;
    if (version !== BRIDGE_PROTOCOL_VERSION) return null;
    return { port, token, version };
  } catch {
    return null;
  }
}

/** The auth message — the FIRST and only thing the extension sends. */
export function authMessage(token: string): string {
  return JSON.stringify({ type: "auth", token });
}

/** The inbound message set (server → extension). */
export type BridgeMessage =
  | { type: "hello"; app: string; version: number }
  | { type: "file-editing"; callId: string; path: string }
  | { type: "file-edited"; callId: string; ok: boolean }
  | { type: "diff"; callId: string; report: string }
  | {
      type: "run";
      phase: "started" | "output" | "done";
      callId: string;
      command?: string;
      chunk?: string;
      ok?: boolean;
    }
  | { type: "run-state"; phase: string }
  | { type: "debug-request"; config: string | null };

/** Parse one frame; null for malformed frames and unknown types (fail
 *  quiet — a newer app may speak messages this build does not). */
export function parseMessage(raw: string): BridgeMessage | null {
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch {
    return null;
  }
  if (typeof value !== "object" || value === null) return null;
  const message = value as Record<string, unknown>;
  switch (message.type) {
    case "hello":
      return typeof message.version === "number"
        ? { type: "hello", app: String(message.app ?? ""), version: message.version }
        : null;
    case "file-editing":
      return typeof message.path === "string" && typeof message.callId === "string"
        ? { type: "file-editing", callId: message.callId, path: message.path }
        : null;
    case "file-edited":
      return typeof message.callId === "string"
        ? { type: "file-edited", callId: message.callId, ok: message.ok === true }
        : null;
    case "diff":
      return typeof message.report === "string" && typeof message.callId === "string"
        ? { type: "diff", callId: message.callId, report: message.report }
        : null;
    case "run": {
      const phase = message.phase;
      if (phase !== "started" && phase !== "output" && phase !== "done") return null;
      if (typeof message.callId !== "string") return null;
      return {
        type: "run",
        phase,
        callId: message.callId,
        command: typeof message.command === "string" ? message.command : undefined,
        chunk: typeof message.chunk === "string" ? message.chunk : undefined,
        ok: typeof message.ok === "boolean" ? message.ok : undefined,
      };
    }
    case "run-state":
      return typeof message.phase === "string"
        ? { type: "run-state", phase: message.phase }
        : null;
    case "debug-request":
      return {
        type: "debug-request",
        config: typeof message.config === "string" ? message.config : null,
      };
    default:
      return null;
  }
}
