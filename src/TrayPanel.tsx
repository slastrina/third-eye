// Tray dropdown panel webview (?view=tray-panel): the designed left-click
// surface — status header, pause/resume, real memory stats, latest
// memories, and the Summon / Memory / Settings actions. All lifecycle logic
// is pure (tray-panel-state.ts); this component fires the IPC and owns the
// timed-pause timer (reliable here: the window lives as long as the app).
import { useEffect, useReducer, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  capturePermissionStatus,
  hidArmedStatus,
  openCaptureSettings,
  openInputSettings,
} from "./chat";
import {
  onWatcherState,
  setWatcherEnabled,
  watcherStatus,
} from "./watcher-state";
import { memoryList, memoryStatus } from "./memory-state";
import {
  PAUSE_OPTIONS,
  initialTrayPanelState,
  pauseMs,
  permissionIssues,
  trayEye,
  trayPanelReducer,
  traySub,
  trayTitle,
  type PauseChoice,
} from "./tray-panel-state";
import { Button } from "./ui/Button";
import { Chip } from "./ui/Chip";
import { EyeIcon } from "./ui/EyeIcon";
import { Panel } from "./ui/Panel";
import { SectionLabel } from "./ui/SectionLabel";
import "./tray-panel.css";

export function TrayPanel() {
  const [panel, dispatch] = useReducer(trayPanelReducer, initialTrayPanelState);
  const resumeTimer = useRef<number | null>(null);

  useEffect(() => {
    let cancelled = false;
    const refresh = () => {
      watcherStatus().then(
        (status) => {
          if (!cancelled) dispatch({ type: "watcher", status });
        },
        (err) => console.debug("tray-panel: watcher_status unavailable:", err),
      );
      memoryStatus().then(
        (status) => {
          if (!cancelled) dispatch({ type: "memory", status });
        },
        (err) => console.debug("tray-panel: memory_status unavailable:", err),
      );
      memoryList(2, 0).then(
        (records) => {
          if (!cancelled) dispatch({ type: "latest", records });
        },
        (err) => console.debug("tray-panel: memory_list unavailable:", err),
      );
      // Live permission health — the panel's front-and-center check, re-run
      // on every open (visibilitychange) so a grant made in System Settings
      // reflects the next time the tray is clicked.
      capturePermissionStatus().then(
        (permission) => {
          if (!cancelled) dispatch({ type: "capture-permission", permission });
        },
        (err) => console.debug("tray-panel: capture_permission_status unavailable:", err),
      );
      hidArmedStatus().then(
        (status) => {
          if (!cancelled) dispatch({ type: "hid-status", status });
        },
        (err) => console.debug("tray-panel: hid_armed_status unavailable:", err),
      );
    };
    refresh();
    // Stats/latest refresh whenever the window is re-shown (tray click).
    const onFocusish = () => {
      if (document.visibilityState === "visible") refresh();
    };
    document.addEventListener("visibilitychange", onFocusish);
    const unlisten = onWatcherState((status) => dispatch({ type: "watcher", status }));
    unlisten.catch((err) => console.error("tray-panel: event subscription failed:", err));
    return () => {
      cancelled = true;
      document.removeEventListener("visibilitychange", onFocusish);
      unlisten.then((f) => f());
    };
  }, []);

  const clearResumeTimer = () => {
    if (resumeTimer.current !== null) {
      window.clearTimeout(resumeTimer.current);
      resumeTimer.current = null;
    }
  };

  const pause = (choice: PauseChoice) => {
    dispatch({ type: "paused", choice, now: Date.now() });
    clearResumeTimer();
    setWatcherEnabled(false).then(
      (status) => dispatch({ type: "watcher", status }),
      (err) => console.debug("tray-panel: set_watcher_enabled unavailable:", err),
    );
    const ms = pauseMs(choice);
    if (ms !== null) {
      resumeTimer.current = window.setTimeout(() => {
        setWatcherEnabled(true).then(
          (status) => dispatch({ type: "watcher", status }),
          (err) => console.debug("tray-panel: timed resume unavailable:", err),
        );
      }, ms);
    }
  };

  const resume = () => {
    clearResumeTimer();
    setWatcherEnabled(true).then(
      (status) => dispatch({ type: "watcher", status }),
      (err) => console.debug("tray-panel: set_watcher_enabled unavailable:", err),
    );
  };

  const close = () => {
    invoke("hide_tray_panel").catch((err) =>
      console.debug("tray-panel: hide_tray_panel unavailable:", err),
    );
  };

  const summon = () => {
    close();
    invoke("show_overlay")
      .then(() => invoke("focus_overlay"))
      .catch((err) => console.debug("tray-panel: summon unavailable:", err));
  };

  const openSettings = () => {
    close();
    invoke("show_settings_window").catch((err) =>
      console.debug("tray-panel: show_settings_window unavailable:", err),
    );
  };

  return (
    <div className="tray-panel-root">
      <Panel variant="glass" className="tray-panel-card">
        <div className="tray-panel-header">
          <EyeIcon state={trayEye(panel)} size={34} stroke="#ffffff" />
          <div className="tray-panel-header-text">
            <div className="tray-panel-title">{trayTitle(panel)}</div>
            <div className="tray-panel-sub">{traySub(panel, Date.now())}</div>
          </div>
          <button type="button" className="tray-panel-close" aria-label="Close" onClick={close}>
            ✕
          </button>
        </div>

        {permissionIssues(panel).map((issue) => (
          <div key={issue.key} className="tray-panel-issue" role="alert">
            <div className="tray-panel-issue-text">
              <strong>{issue.title}</strong>
              <span>{issue.detail}</span>
            </div>
            <Button
              variant="outline"
              onClick={() => {
                (issue.key === "screen" ? openCaptureSettings() : openInputSettings()).catch(
                  (err) => console.debug("tray-panel: open settings no-op:", err),
                );
              }}
            >
              Open System Settings
            </Button>
          </div>
        ))}

                {panel.watching === true && (
          <div className="tray-panel-section">
            <SectionLabel>Pause watching</SectionLabel>
            <div className="tray-panel-pause-row">
              {PAUSE_OPTIONS.map((option) => (
                <Chip key={option.value} onClick={() => pause(option.value)}>
                  {option.label}
                </Chip>
              ))}
            </div>
          </div>
        )}
        {panel.watching === false && (
          <div className="tray-panel-section">
            <Button variant="primary" className="tray-panel-resume" onClick={resume}>
              Resume watching
            </Button>
          </div>
        )}

        {panel.memoriesStored !== null && (
          <div className="tray-panel-section tray-panel-stats">
            <div>
              <div className="tray-panel-stat-value">{panel.memoriesStored}</div>
              <div className="tray-panel-stat-label">memories stored</div>
            </div>
          </div>
        )}

        {panel.latest.length > 0 && (
          <div className="tray-panel-section">
            <SectionLabel>Latest</SectionLabel>
            <div className="tray-panel-latest">
              {panel.latest.map((record) => (
                <div key={record.id} className="tray-panel-latest-row">
                  <span className="tray-panel-latest-dot" aria-hidden="true" />
                  <span className="tray-panel-latest-text">{record.summary}</span>
                </div>
              ))}
            </div>
          </div>
        )}

        <div className="tray-panel-actions">
          <Button variant="accent" onClick={summon}>
            Summon
          </Button>
          <Button
            variant="outline"
            onClick={() => {
              close();
              invoke("show_memory_window").catch((err) =>
                console.debug("tray-panel: show_memory_window unavailable:", err),
              );
            }}
          >
            Memory
          </Button>
          <Button variant="outline" onClick={openSettings}>
            Settings
          </Button>
        </div>
      </Panel>
    </div>
  );
}
