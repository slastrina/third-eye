// Third Eye VS Code extension (coding-agent S7): a thin, read-mostly
// window into the agent's coding work. It connects to the app's
// loopback-only bridge (discovered via bridge.json in Third Eye's
// app-data folder), authenticates with the per-boot token, and then:
//
// - opens/reveals files as the agent edits them (with a brief "Third Eye
//   edited this" highlight),
// - shows the agent's workspace diff on arrival (and on demand via the
//   "Third Eye: Show Last Workspace Diff" command),
// - mirrors run_in_workspace lifecycle in the status bar,
// - starts a debug session ONLY after the user clicks Allow on the
//   in-VS-Code prompt (the agent can request, never start).
//
// The extension never writes files and never sends anything to the app
// beyond the auth frame.

import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import * as vscode from "vscode";
import WebSocket from "ws";
import {
  authMessage,
  discoveryCandidates,
  parseDiscovery,
  parseMessage,
  BridgeMessage,
} from "./protocol";

const RECONNECT_DELAY_MS = 10_000;
const HIGHLIGHT_MS = 3_000;

let socket: WebSocket | null = null;
let statusBar: vscode.StatusBarItem;
let lastDiff = "";
let reconnectTimer: NodeJS.Timeout | null = null;
let disposed = false;

const editDecoration = vscode.window.createTextEditorDecorationType({
  backgroundColor: new vscode.ThemeColor("diffEditor.insertedTextBackground"),
  isWholeLine: true,
  overviewRulerColor: new vscode.ThemeColor("charts.green"),
});

const diffScheme = "third-eye-diff";

class DiffProvider implements vscode.TextDocumentContentProvider {
  private emitter = new vscode.EventEmitter<vscode.Uri>();
  onDidChange = this.emitter.event;
  provideTextDocumentContent(): string {
    return lastDiff || "No diff received from Third Eye yet.";
  }
  refresh(uri: vscode.Uri): void {
    this.emitter.fire(uri);
  }
}

const diffProvider = new DiffProvider();
const diffUri = vscode.Uri.parse(`${diffScheme}:Third Eye Diff.diff`);

export function activate(context: vscode.ExtensionContext): void {
  disposed = false;
  statusBar = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 50);
  statusBar.text = "$(eye-closed) Third Eye";
  statusBar.tooltip = "Third Eye bridge: not connected";
  statusBar.show();
  context.subscriptions.push(statusBar, editDecoration);
  context.subscriptions.push(
    vscode.workspace.registerTextDocumentContentProvider(diffScheme, diffProvider),
    vscode.commands.registerCommand("thirdEye.showDiff", () => showDiff()),
    vscode.commands.registerCommand("thirdEye.reconnect", () => {
      closeSocket();
      connect();
    }),
  );
  connect();
}

export function deactivate(): void {
  disposed = true;
  if (reconnectTimer) clearTimeout(reconnectTimer);
  closeSocket();
}

function closeSocket(): void {
  if (socket) {
    try {
      socket.close();
    } catch {
      // Already closing/closed.
    }
    socket = null;
  }
}

function scheduleReconnect(): void {
  if (disposed || reconnectTimer) return;
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null;
    connect();
  }, RECONNECT_DELAY_MS);
}

/** Read the freshest discovery file, if the app is running. */
function readDiscovery(): { port: number; token: string } | null {
  for (const candidate of discoveryCandidates(process.platform, os.homedir(), process.env)) {
    try {
      const parsed = parseDiscovery(fs.readFileSync(candidate, "utf8"));
      if (parsed) return parsed;
    } catch {
      // Absent file — the app is not running or has never run.
    }
  }
  return null;
}

function connect(): void {
  if (disposed || socket) return;
  const discovery = readDiscovery();
  if (!discovery) {
    statusBar.text = "$(eye-closed) Third Eye";
    statusBar.tooltip = "Third Eye bridge: app not running (no bridge.json)";
    scheduleReconnect();
    return;
  }
  const ws = new WebSocket(`ws://127.0.0.1:${discovery.port}`);
  socket = ws;
  ws.on("open", () => ws.send(authMessage(discovery.token)));
  ws.on("message", (data) => {
    const message = parseMessage(String(data));
    if (message) void handle(message);
  });
  ws.on("close", () => {
    if (socket === ws) socket = null;
    statusBar.text = "$(eye-closed) Third Eye";
    statusBar.tooltip = "Third Eye bridge: disconnected";
    scheduleReconnect();
  });
  ws.on("error", () => {
    // close fires next; the reconnect loop handles it.
  });
}

async function handle(message: BridgeMessage): Promise<void> {
  switch (message.type) {
    case "hello":
      statusBar.text = "$(eye) Third Eye";
      statusBar.tooltip = "Third Eye bridge: connected";
      return;
    case "file-editing":
      return revealFile(message.path);
    case "file-edited":
      // Files change on disk; VS Code reloads clean editors automatically.
      return;
    case "diff":
      lastDiff = message.report;
      diffProvider.refresh(diffUri);
      return showDiff();
    case "run":
      if (message.phase === "started") {
        statusBar.text = `$(sync~spin) ${truncate(message.command ?? "running", 32)}`;
        statusBar.tooltip = `Third Eye is running: ${message.command ?? ""}`;
      } else if (message.phase === "done") {
        statusBar.text = message.ok ? "$(check) Third Eye" : "$(error) Third Eye";
        statusBar.tooltip = message.ok
          ? "Third Eye: last command succeeded"
          : "Third Eye: last command failed";
      }
      return;
    case "run-state":
      if (message.phase !== "running") {
        if (statusBar.text.startsWith("$(sync~spin)")) statusBar.text = "$(eye) Third Eye";
      }
      return;
    case "debug-request":
      return handleDebugRequest(message.config);
  }
}

function truncate(text: string, max: number): string {
  return text.length > max ? `${text.slice(0, max)}…` : text;
}

/** Resolve a (possibly workspace-relative) path and reveal it, briefly
 *  highlighting the file as agent-edited. */
async function revealFile(rawPath: string): Promise<void> {
  const resolved = resolveInWorkspace(rawPath);
  if (!resolved) return;
  try {
    const doc = await vscode.workspace.openTextDocument(resolved);
    const editor = await vscode.window.showTextDocument(doc, {
      preview: true,
      preserveFocus: true,
    });
    const fullRange = new vscode.Range(0, 0, Math.max(doc.lineCount - 1, 0), 0);
    editor.setDecorations(editDecoration, [fullRange]);
    setTimeout(() => {
      // The editor may have closed meanwhile; setDecorations on a disposed
      // editor throws, so re-find it.
      const live = vscode.window.visibleTextEditors.find(
        (e) => e.document.uri.toString() === doc.uri.toString(),
      );
      live?.setDecorations(editDecoration, []);
    }, HIGHLIGHT_MS);
  } catch {
    // The file may not exist yet (first write in flight) — the next
    // file-editing event after creation will land.
  }
}

function resolveInWorkspace(rawPath: string): vscode.Uri | null {
  if (path.isAbsolute(rawPath)) return vscode.Uri.file(rawPath);
  for (const folder of vscode.workspace.workspaceFolders ?? []) {
    const joined = path.join(folder.uri.fsPath, rawPath);
    if (fs.existsSync(joined)) return vscode.Uri.file(joined);
  }
  const first = vscode.workspace.workspaceFolders?.[0];
  return first ? vscode.Uri.file(path.join(first.uri.fsPath, rawPath)) : null;
}

async function showDiff(): Promise<void> {
  const doc = await vscode.workspace.openTextDocument(diffUri);
  await vscode.languages.setTextDocumentLanguage(doc, "diff");
  await vscode.window.showTextDocument(doc, { preview: true, preserveFocus: true });
}

/** The user gate for agent-requested debugging: nothing starts without an
 *  explicit Allow click in VS Code. */
async function handleDebugRequest(config: string | null): Promise<void> {
  const folder = vscode.workspace.workspaceFolders?.[0];
  if (!folder) {
    void vscode.window.showWarningMessage(
      "Third Eye requested a debug session, but no folder is open.",
    );
    return;
  }
  const label = config ? `configuration "${config}"` : "the default configuration";
  const choice = await vscode.window.showInformationMessage(
    `Third Eye wants to start a debug session (${label}).`,
    "Allow",
    "Deny",
  );
  if (choice !== "Allow") return;
  let target = config ?? undefined;
  if (!target) {
    const configs =
      vscode.workspace
        .getConfiguration("launch", folder.uri)
        .get<{ name?: string }[]>("configurations") ?? [];
    target = configs[0]?.name;
  }
  if (!target) {
    void vscode.window.showWarningMessage(
      "Third Eye: no launch configuration found in .vscode/launch.json.",
    );
    return;
  }
  const started = await vscode.debug.startDebugging(folder, target);
  if (!started) {
    void vscode.window.showWarningMessage(
      "Third Eye: the debug session could not be started (check launch.json).",
    );
  }
}
