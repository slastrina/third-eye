import { useEffect, useReducer, useRef, useState } from "react";
import {
  bannerDetail,
  bannerTitle,
  capturePermissionStatus,
  captureErrorDetail,
  captureErrorTitle,
  captureScreen,
  chatReducer,
  composeMessages,
  initialChatState,
  modelInfo,
  onLlmDone,
  onLlmError,
  onLlmToken,
  onLlmToolCall,
  onLlmToolResult,
  onModelInfoBroadcast,
  onNudgeDismiss,
  onNudgeShow,
  onPrivacyChanged,
  onRunState,
  openCaptureSettings,
  privacyStatus,
  runState,
  sendChat,
  setModel,
  showStopButton,
  startHealthProbe,
  stopChat,
  stripFailedTail,
  toCaptureFlowError,
} from "./chat";
import {
  hideOverlay,
  onOverlayStateChanged,
  type OverlayState,
} from "./overlay-state";
import { onTrayNotice, type TrayNotice } from "./tray-notice";

function App() {
  const [state, setState] = useState<OverlayState>("hidden");
  // Event handlers registered once need the live state without re-binding.
  const stateRef = useRef(state);
  stateRef.current = state;
  const inputRef = useRef<HTMLInputElement>(null);

  const [chat, dispatchChat] = useReducer(chatReducer, initialChatState);
  const chatRef = useRef(chat);
  chatRef.current = chat;
  const [draft, setDraft] = useState("");
  const messagesRef = useRef<HTMLDivElement>(null);

  // Token deltas are coalesced per animation frame so a fast stream costs at
  // most one render per frame, not one per token. Terminal events carry the
  // authoritative full text, so a tail left in this buffer is harmless — the
  // reducer drops it as stale once the request id is cleared.
  const tokenBufferRef = useRef(new Map<number, string>());
  const rafRef = useRef<number | null>(null);

  useEffect(() => {
    const unlisten = onOverlayStateChanged(setState);
    // MEM115: a capability/ACL denial rejects listen() inside the real app —
    // catch loudly so a dead subscription is visible, not a frozen surface.
    unlisten.catch((err) => console.error("overlay: event subscription failed:", err));
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  // Transient banner for tray notices. Since S07 no menu entry maps to a
  // stub anymore, but the plumbing stays for future transient notices — an
  // unknown feature id still renders a named banner, then fades on a timer.
  const [trayNotice, setTrayNotice] = useState<TrayNotice | null>(null);
  const trayNoticeTimer = useRef<number | null>(null);
  useEffect(() => {
    const unlisten = onTrayNotice((notice) => {
      setTrayNotice(notice);
      if (trayNoticeTimer.current !== null) window.clearTimeout(trayNoticeTimer.current);
      trayNoticeTimer.current = window.setTimeout(() => setTrayNotice(null), 6000);
    });
    unlisten.catch((err) => console.error("overlay: event subscription failed:", err));
    return () => {
      unlisten.then((f) => f());
      if (trayNoticeTimer.current !== null) window.clearTimeout(trayNoticeTimer.current);
    };
  }, []);

  useEffect(() => {
    if (state === "visible-focused") {
      inputRef.current?.focus();
    } else {
      inputRef.current?.blur();
    }
  }, [state]);

  useEffect(() => {
    const dismiss = () => {
      if (stateRef.current === "hidden") return;
      // Optimistic: the Rust event will confirm, but marking hidden now
      // stops Escape+blur firing back-to-back from double-invoking hide.
      setState("hidden");
      hideOverlay().catch((err) => {
        // A rejected Hide here means Rust already hid the panel (e.g. the
        // global hotkey raced us) — benign, but keep it visible for debugging.
        console.debug("overlay: dismiss no-op:", err);
      });
    };
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") dismiss();
    };
    // The webview only holds key focus in visible-focused mode, so blur there
    // means the user moved on — dismiss. The panel resigning key because Rust
    // already hid it also fires blur; the hidden guard in dismiss absorbs it.
    const onBlur = () => {
      if (stateRef.current === "visible-focused") dismiss();
    };
    window.addEventListener("keydown", onKeyDown);
    window.addEventListener("blur", onBlur);
    return () => {
      window.removeEventListener("keydown", onKeyDown);
      window.removeEventListener("blur", onBlur);
    };
  }, []);

  useEffect(() => {
    const flushTokens = () => {
      rafRef.current = null;
      const buffered = tokenBufferRef.current;
      tokenBufferRef.current = new Map();
      for (const [requestId, token] of buffered) {
        dispatchChat({ type: "token", payload: { requestId, token } });
      }
    };
    const unlistens = [
      onLlmToken((payload) => {
        const buffer = tokenBufferRef.current;
        buffer.set(payload.requestId, (buffer.get(payload.requestId) ?? "") + payload.token);
        if (rafRef.current === null) {
          rafRef.current = requestAnimationFrame(flushTokens);
        }
      }),
      onLlmDone((payload) => dispatchChat({ type: "done", payload })),
      onLlmError((payload) => dispatchChat({ type: "error", payload })),
      // Tool phases (S03) dispatch directly like terminal events — they carry
      // no text, so the frame-coalesced token buffer can't reorder anything
      // user-visible past them.
      onLlmToolCall((payload) => dispatchChat({ type: "tool-call", payload })),
      onLlmToolResult((payload) => dispatchChat({ type: "tool-result", payload })),
    ];
    unlistens.forEach((u) => {
      u.catch((err) => console.error("overlay: event subscription failed:", err));
    });
    return () => {
      unlistens.forEach((u) => u.then((f) => f()));
      if (rafRef.current !== null) cancelAnimationFrame(rafRef.current);
    };
  }, []);

  // Automatic reconnect probing while an error banner is up: exponential
  // backoff 2s → 30s cap until llm_health reports online, then it stops.
  const probing = chat.banner !== null && !chat.banner.online;
  useEffect(() => {
    if (!probing) return;
    return startHealthProbe((health) =>
      dispatchChat({ type: "health", online: health.online }),
    );
  }, [probing]);

  useEffect(() => {
    messagesRef.current?.scrollTo({ top: messagesRef.current.scrollHeight });
  }, [chat.messages]);

  // Routing state for the model indicator. Outside a Tauri runtime (vite dev
  // in a plain browser) the invoke rejects and the indicator simply stays
  // hidden — never a crash.
  useEffect(() => {
    let cancelled = false;
    modelInfo().then(
      (info) => {
        if (!cancelled) dispatchChat({ type: "model-info", info });
      },
      (err) => console.debug("llm: model_info unavailable:", err),
    );
    return () => {
      cancelled = true;
    };
  }, []);

  // Chat run-state snapshot for the Stop control (S04 T04). Outside a Tauri
  // runtime the invoke rejects and the control simply stays hidden (idle) —
  // never a crash (same absorb posture as model_info above).
  useEffect(() => {
    let cancelled = false;
    runState().then(
      (payload) => {
        if (!cancelled) dispatchChat({ type: "run-state", phase: payload.phase });
      },
      (err) => console.debug("llm: run_state unavailable:", err),
    );
    return () => {
      cancelled = true;
    };
  }, []);

  // Screen Recording permission snapshot for the attach affordance. Outside
  // a Tauri runtime the invoke rejects and the button simply renders in its
  // default state — never a crash (same posture as model_info above).
  useEffect(() => {
    let cancelled = false;
    capturePermissionStatus().then(
      (permission) => {
        if (!cancelled) dispatchChat({ type: "capture-permission", permission });
      },
      (err) => console.debug("capture: permission status unavailable:", err),
    );
    return () => {
      cancelled = true;
    };
  }, []);

  // Privacy-mode snapshot behind the attach affordance's hint (same absorb
  // posture as the permission query above).
  useEffect(() => {
    let cancelled = false;
    privacyStatus().then(
      (status) => {
        if (!cancelled) dispatchChat({ type: "privacy", status });
      },
      (err) => console.debug("capture: privacy status unavailable:", err),
    );
    return () => {
      cancelled = true;
    };
  }, []);

  // Cross-window sync (S07): mutation responses only reach the calling
  // window, so routing changes made in the settings window arrive here via
  // the llm://model-info broadcast, and privacy toggles (settings window or
  // tray) via the capture://privacy broadcast — the indicator and attach
  // affordance stay truthful without polling.
  useEffect(() => {
    const unlistens = [
      onModelInfoBroadcast((info) => dispatchChat({ type: "model-info", info })),
      onPrivacyChanged((status) => dispatchChat({ type: "privacy", status })),
      // Run-state (S04 T04): the backend broadcasts running/stopped/idle so the
      // Stop control tracks the in-flight run without polling.
      onRunState((payload) => dispatchChat({ type: "run-state", phase: payload.phase })),
    ];
    unlistens.forEach((u) => {
      u.catch((err) => console.error("overlay: event subscription failed:", err));
    });
    return () => {
      unlistens.forEach((u) => u.then((f) => f()));
    };
  }, []);

  // Nudge lifecycle (S05): the backend parks a nudge on the idle overlay via
  // nudge://show and takes it down via nudge://dismiss — a "summoned" dismiss
  // (hotkey pressed on the banner) stages the payload as the next submit's
  // context preload in the reducer. Self-dismissing by contract: the banner
  // renders purely from chat.nudge, so the dismiss event is the whole story.
  useEffect(() => {
    const unlistens = [
      onNudgeShow((payload) => dispatchChat({ type: "nudge-shown", payload })),
      onNudgeDismiss((reason) => dispatchChat({ type: "nudge-dismissed", reason })),
    ];
    unlistens.forEach((u) => {
      u.catch((err) => console.error("overlay: event subscription failed:", err));
    });
    return () => {
      unlistens.forEach((u) => u.then((f) => f()));
    };
  }, []);

  // R007: walkthrough opens are observable without a debugger attached.
  const walkthroughOpen = chat.captureError?.kind === "permission-denied";
  useEffect(() => {
    if (walkthroughOpen) console.debug("capture: permission walkthrough opened");
  }, [walkthroughOpen]);

  const attachScreen = () => {
    if (chatRef.current.attachPending) return;
    dispatchChat({ type: "attach-start" });
    captureScreen().then(
      (frame) => dispatchChat({ type: "attach-done", frame }),
      (err) => dispatchChat({ type: "attach-error", error: toCaptureFlowError(err) }),
    );
  };

  // Stop the in-flight run. The backend broadcasts the resulting run-state, but
  // apply the returned phase directly too so the control clears without waiting
  // for the round-trip (never rejects backend-side — an idle stop is a no-op).
  const stopRun = () => {
    stopChat().then(
      (payload) => dispatchChat({ type: "run-state", phase: payload.phase }),
      (err) => console.warn("llm: stop_chat failed:", err),
    );
  };

  const openScreenRecordingSettings = () => {
    console.debug("capture: opening Screen Recording settings from walkthrough");
    openCaptureSettings().catch((err) =>
      console.warn("capture: open settings failed:", err),
    );
  };

  const overrideLane = (lane: string) => {
    setModel(lane).then(
      (info) => dispatchChat({ type: "model-info", info }),
      // Rejection means the backend left routing unchanged — keep showing
      // the last known state rather than guessing.
      (err) => console.warn("llm: set_model failed:", err),
    );
  };

  const submit = (question: string, retry = false) => {
    const trimmed = question.trim();
    if (!trimmed) return;
    const base = retry ? stripFailedTail(chatRef.current.messages) : chatRef.current.messages;
    // The staged frame rides this message; the submit action consumes it, so
    // a retry after a failure re-asks the question without the screenshot.
    const staged = chatRef.current.attachment;
    // A summon-from-nudge preload grounds exactly this question; the submit
    // action consumes it reducer-side.
    const history = composeMessages(
      base,
      trimmed,
      staged ? [{ base64Png: staged.base64Png }] : [],
      chatRef.current.nudgePreload,
    );
    dispatchChat({ type: "submit", question: trimmed, retry });
    sendChat(history).then(
      (requestId) => dispatchChat({ type: "request-started", requestId }),
      (err) => dispatchChat({ type: "request-failed", detail: String(err) }),
    );
  };

  const onSubmit = (event: React.FormEvent) => {
    event.preventDefault();
    if (!draft.trim()) return;
    submit(draft);
    setDraft("");
  };

  // Const binding so the non-null narrowing survives into the JSX callbacks.
  const routing = chat.modelInfo;
  const activeModelId = routing
    ? routing.lanes.find((lane) => lane.name === routing.activeLane)?.modelId
    : null;

  // Idle-because-of-a-nudge renders ONLY the small edge banner — the full
  // chat chrome would read as a ghost panel parked over the user's work. In
  // visible-focused (summoned) or plain visible-idle the panel is unchanged.
  const parkedNudge = state === "visible-idle" ? chat.nudge : null;

  if (parkedNudge) {
    return (
      <div className="overlay-root" data-state={state} data-nudge="true">
        <div className="nudge-banner" role="status" aria-live="polite">
          <span className="nudge-dot" aria-hidden="true" />
          <span className="nudge-message">{parkedNudge.message}</span>
          <span className="nudge-hint">press the hotkey to ask</span>
        </div>
      </div>
    );
  }

  return (
    <div className="overlay-root" data-state={state}>
      <div className="overlay-panel">
        <form onSubmit={onSubmit}>
          <input
            ref={inputRef}
            className="overlay-input"
            type="text"
            placeholder="Third Eye"
            aria-label="Overlay input"
            value={draft}
            onChange={(event) => setDraft(event.target.value)}
          />
        </form>
        {showStopButton(chat) && (
          <div className="run-controls">
            <button
              type="button"
              className="chat-stop"
              aria-label="Stop the running task"
              onClick={stopRun}
            >
              Stop
            </button>
          </div>
        )}
        {trayNotice && (
          <div className="tray-notice" role="status">
            <div className="chat-banner-text">
              <strong>{trayNotice.title}</strong>
              <span>{trayNotice.detail}</span>
            </div>
            <button type="button" className="chat-retry" onClick={() => setTrayNotice(null)}>
              OK
            </button>
          </div>
        )}
        {/* Attach affordance: hidden only when the platform reports no
            capture backend at all (supported === false). */}
        {chat.capturePermission?.supported !== false && (
          <div className="attach-row">
            {chat.attachment ? (
              <span className="attach-chip">
                Screen attached · {chat.attachment.width}×{chat.attachment.height}
                <button
                  type="button"
                  className="attach-chip-clear"
                  aria-label="Remove screen attachment"
                  onClick={() => dispatchChat({ type: "attach-clear" })}
                >
                  ×
                </button>
              </span>
            ) : (
              <button
                type="button"
                className="attach-button"
                disabled={chat.attachPending}
                onClick={attachScreen}
              >
                {chat.attachPending && <span className="attach-spinner" aria-hidden="true" />}
                {chat.attachPending ? "Capturing…" : "Attach my screen"}
              </button>
            )}
            {/* Privacy hint only — the button stays live so an attempted
                capture still surfaces the typed privacy-mode error. */}
            {chat.privacy?.enabled && (
              <span className="attach-privacy-hint" title="Turn Privacy Mode off in the tray menu or settings">
                Privacy Mode on — capture blocked
              </span>
            )}
          </div>
        )}
        {chat.captureError &&
          (chat.captureError.kind === "permission-denied" ? (
            <div className="capture-walkthrough" role="alert">
              <strong>{captureErrorTitle(chat.captureError)}</strong>
              <ol className="capture-walkthrough-steps">
                <li>Open System Settings below — it lands on Privacy &amp; Security → Screen Recording.</li>
                <li>Turn on Third Eye in the list (macOS may ask to relaunch the app).</li>
                <li>Come back and press Try again.</li>
              </ol>
              <div className="capture-walkthrough-actions">
                <button type="button" className="chat-retry" onClick={openScreenRecordingSettings}>
                  Open System Settings
                </button>
                <button type="button" className="chat-retry" onClick={attachScreen}>
                  Try again
                </button>
                <button
                  type="button"
                  className="chat-retry"
                  onClick={() => dispatchChat({ type: "attach-clear" })}
                >
                  Dismiss
                </button>
              </div>
            </div>
          ) : (
            <div className="chat-banner" role="alert">
              <div className="chat-banner-text">
                <strong>{captureErrorTitle(chat.captureError)}</strong>
                <span>{captureErrorDetail(chat.captureError)}</span>
              </div>
              <button type="button" className="chat-retry" onClick={attachScreen}>
                Try again
              </button>
            </div>
          ))}
        {chat.banner && (
          <div
            className="chat-banner"
            data-online={chat.banner.online}
            data-kind={chat.banner.error.kind}
            role="alert"
          >
            <div className="chat-banner-text">
              <strong>
                {chat.banner.online
                  ? "Local AI back online"
                  : bannerTitle(chat.banner.error)}
              </strong>
              <span>{bannerDetail(chat.banner.error)}</span>
            </div>
            <button
              type="button"
              className="chat-retry"
              disabled={chat.lastQuestion === null}
              onClick={() => submit(chat.lastQuestion ?? "", true)}
            >
              Retry
            </button>
          </div>
        )}
        {chat.messages.length > 0 && (
          <div className="chat-messages" ref={messagesRef}>
            {chat.messages.map((message, index) => (
              <div
                key={index}
                className={`chat-message chat-${message.role}`}
                data-status={message.status}
              >
                <span className="chat-text">{message.text}</span>
                {message.role === "user" && message.attached && (
                  <span className="chat-attached-tag" title="A screenshot rode this message">
                    screen
                  </span>
                )}
                {message.role === "assistant" && message.memory && (
                  <span
                    className="chat-memory-tag"
                    data-phase={message.memory}
                    title={
                      message.memory === "searching"
                        ? "The model is searching your stored memories"
                        : "This answer consulted your stored memories"
                    }
                  >
                    {message.memory === "searching" ? "searching memory…" : "memory consulted"}
                  </span>
                )}
                {message.role === "assistant" && message.status === "streaming" && (
                  <span className="chat-caret" aria-label="Answer streaming" />
                )}
                {message.role === "assistant" && message.status === "interrupted" && (
                  <span className="chat-interrupted-tag">interrupted</span>
                )}
              </div>
            ))}
          </div>
        )}
        {routing && (
          <div className="model-indicator" data-lane={routing.activeLane}>
            <span className="model-indicator-model" title={routing.endpoint}>
              {/* An unpinned lane means the endpoint serves its default model. */}
              {activeModelId ?? "endpoint default model"}
            </span>
            <div className="model-lanes" role="group" aria-label="Model lane override">
              {routing.lanes.map((lane) => (
                <button
                  key={lane.name}
                  type="button"
                  className="model-lane"
                  aria-pressed={lane.name === routing.activeLane}
                  title={lane.modelId ?? "endpoint default model"}
                  onClick={() => overrideLane(lane.name)}
                >
                  {lane.name}
                </button>
              ))}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

export default App;
