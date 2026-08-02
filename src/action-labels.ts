// Human labels for tool calls — shared by the HUD trail (hud-state.ts)
// and the chat transcript's steps block (chat.ts). Pure; the arguments
// string is the model's raw JSON and malformed input falls back to the
// bare tool name.

/** Derive the label + ghost target from one tool call. */
export function describeCall(name: string, rawArguments: string): {
  label: string;
  input: boolean;
  target: { x: number; y: number } | null;
} {
  let args: Record<string, unknown> = {};
  try {
    const parsed: unknown = JSON.parse(rawArguments);
    if (parsed && typeof parsed === "object") args = parsed as Record<string, unknown>;
  } catch {
    // Malformed arguments still execute-and-fail loop-side; label the tool.
  }
  if (name === "input_action") {
    const action = typeof args.action === "string" ? args.action : "";
    const x = typeof args.x === "number" ? args.x : null;
    const y = typeof args.y === "number" ? args.y : null;
    const target = x !== null && y !== null ? { x, y } : null;
    switch (action) {
      case "mouse-move":
        return { label: target ? `move · ${x}, ${y}` : "move the mouse", input: true, target };
      case "mouse-click": {
        const count = typeof args.clicks === "number" ? args.clicks : 1;
        const verb = count === 3 ? "triple-click" : count === 2 ? "double-click" : "click";
        return { label: target ? `${verb} · ${x}, ${y}` : verb, input: true, target };
      }
      case "mouse-drag": {
        const toX = typeof args.toX === "number" ? args.toX : null;
        const toY = typeof args.toY === "number" ? args.toY : null;
        const fromX = typeof args.fromX === "number" ? args.fromX : null;
        const fromY = typeof args.fromY === "number" ? args.fromY : null;
        const dragTarget = toX !== null && toY !== null ? { x: toX, y: toY } : null;
        return {
          label:
            fromX !== null && toX !== null
              ? `drag · ${fromX}, ${fromY} → ${toX}, ${toY}`
              : "drag",
          input: true,
          target: dragTarget,
        };
      }
      case "scroll": {
        const dy = typeof args.deltaY === "number" ? args.deltaY : 0;
        const dx = typeof args.deltaX === "number" ? args.deltaX : 0;
        const dir = dy > 0 ? "down" : dy < 0 ? "up" : dx > 0 ? "right" : "left";
        return { label: `scroll · ${dir}`, input: true, target };
      }
      case "type-text": {
        const text = typeof args.text === "string" ? args.text : "";
        const shown = text.length > 24 ? `${text.slice(0, 24)}…` : text;
        return { label: shown ? `type · “${shown}”` : "type", input: true, target: null };
      }
      case "key-press": {
        const key = typeof args.key === "string" ? args.key : "";
        const mods = Array.isArray(args.modifiers)
          ? (args.modifiers as unknown[]).filter((m): m is string => typeof m === "string")
          : [];
        const combo = mods.length > 0 ? `${mods.join("+")}+${key}` : key;
        return { label: combo ? `press · ${combo}` : "press a key", input: true, target: null };
      }
      default:
        return { label: "input action", input: true, target: null };
    }
  }
  if (name === "run_command" || name === "run_in_workspace") {
    const command = typeof args.command === "string" ? args.command : "";
    const shown = command.length > 40 ? `${command.slice(0, 40)}…` : command;
    // Commands hold the terminal, NOT the pointer: input stays false so a
    // terminal-only run never summons the mouse follower/ghost overlay
    // (user report 2026-08-02) — the pill + trail still show the run.
    const fallback = name === "run_in_workspace" ? "run in workspace" : "run a command";
    return { label: shown ? `run · ${shown}` : fallback, input: false, target: null };
  }
  if (name === "clipboard") {
    const op = typeof args.op === "string" ? args.op : "";
    return {
      label: op === "write" ? "clipboard · write" : "clipboard · read",
      input: true,
      target: null,
    };
  }
  if (name === "wait") {
    const ms = typeof args.ms === "number" ? args.ms : 500;
    return { label: `wait · ${ms}ms`, input: false, target: null };
  }
  if (name === "take_screenshot") {
    return { label: "look at the screen", input: false, target: null };
  }
  if (name === "screen_query") return { label: "read the screen", input: false, target: null };
  if (name === "read_page") return { label: "read the page", input: false, target: null };
  if (name === "memory_search") {
    const query = typeof args.query === "string" ? args.query : "";
    return { label: query ? `recall · “${query}”` : "search memory", input: false, target: null };
  }
  if (name === "remember") {
    const fact = typeof args.fact === "string" ? args.fact : "";
    const shown = fact.length > 30 ? `${fact.slice(0, 30)}…` : fact;
    return { label: shown ? `remember · “${shown}”` : "remember", input: false, target: null };
  }
  if (name === "chat_history_search") {
    const query = typeof args.query === "string" ? args.query : "";
    return {
      label: query ? `past chats · “${query}”` : "search past chats",
      input: false,
      target: null,
    };
  }
  if (name === "read_file" || name === "write_file") {
    const path = typeof args.path === "string" ? args.path : "";
    const file = path.split("/").filter(Boolean).pop() ?? "";
    const verb = name === "read_file" ? "read" : "write";
    return { label: file ? `${verb} · ${file}` : `${verb} a file`, input: false, target: null };
  }
  if (name === "workspace_diff") {
    return { label: "review the diff", input: false, target: null };
  }
  if (name === "vscode_debug") {
    return { label: "ask VS Code to debug", input: false, target: null };
  }
  if (name === "list_dir") {
    const path = typeof args.path === "string" ? args.path : "";
    const dir = path.split("/").filter(Boolean).pop() ?? "";
    return { label: dir ? `list · ${dir}` : "list the workspace", input: false, target: null };
  }
  if (name === "focus_app") {
    // The wire argument is `app` (FocusAppTool's schema); `name` kept as a
    // fallback for older transcripts.
    const app =
      typeof args.app === "string" ? args.app : typeof args.name === "string" ? args.name : "";
    return { label: app ? `focus · ${app}` : "focus an app", input: false, target: null };
  }
  return { label: name.replaceAll("_", " "), input: false, target: null };
}
