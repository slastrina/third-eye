import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import Settings from "./Settings";
import { HudCanvasView, HudPillView } from "./Hud";
import { TrayPanel } from "./TrayPanel";
import { MemoryWindow } from "./Memory";
import "./ui/tokens.css";
import "./styles.css";

// One bundle, four windows: tauri.conf.json routes each window by
// index.html?view= — settings, the two HUD windows (pill + canvas), and
// everything else is the overlay.
const view = new URLSearchParams(window.location.search).get("view");

const root =
  view === "settings" ? (
    <Settings />
  ) : view === "hud-pill" ? (
    <HudPillView />
  ) : view === "hud-canvas" ? (
    <HudCanvasView />
  ) : view === "tray-panel" ? (
    <TrayPanel />
  ) : view === "memory" ? (
    <MemoryWindow />
  ) : (
    <App />
  );

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>{root}</React.StrictMode>,
);
