# Third Eye — VS Code extension

Live visibility into Third Eye's coding agent. While the agent works in a
designated workspace you see, inside VS Code:

- **Files opening as they are edited**, with a brief "edited" highlight.
- **The workspace diff** the agent reviews before declaring a task done
  (auto-opens; also `Third Eye: Show Last Workspace Diff`).
- **Run status** in the status bar while `run_in_workspace` builds/tests.
- **Debug requests**: the agent can *ask* to start a debug session; nothing
  runs until you click **Allow** in VS Code.

## How it connects

Third Eye serves a **loopback-only** WebSocket bridge (`127.0.0.1`, random
port) and writes `bridge.json` — port + a per-boot token, owner-readable
only — into its app-data folder. The extension reads that file, connects,
and authenticates with the token as its first message. The extension never
writes files and sends nothing else to the app.

## Build & install

```sh
cd vscode-extension
npm install
npm test           # protocol unit tests + typecheck
npm run package    # produces third-eye-vscode-<version>.vsix
code --install-extension third-eye-vscode-*.vsix
```

Or from the repo root: `make vsix`.
