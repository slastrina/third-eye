import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import Settings from "./Settings";
import "./styles.css";

// One bundle, two windows: the settings window loads index.html?view=settings
// (see tauri.conf.json) and gets the settings view; everything else is the
// overlay.
const view = new URLSearchParams(window.location.search).get("view");

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    {view === "settings" ? <Settings /> : <App />}
  </React.StrictMode>,
);
