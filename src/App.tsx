import type { ChatSessionSummary } from "./chat";
import { useEffect, useReducer, useRef, useState } from "react";
import {
  bannerDetail,
  bannerTitle,
  capturePermissionStatus,
  captureErrorDetail,
  captureErrorTitle,
  captureScreen,
  chatNewSession,
  chatResumeSession,
  chatSessions,
  chatReducer,
  completeFirstRun,
  composeMessages,
  frameFromImageFile,
  composeContextBlocks,
  type FileContext,
  freshNudgePreload,
  nudgeContextFrame,
  firstRunStatus,
  hotkeyStatus,
  initialChatState,
  isLocalEndpoint,
  memoryRetention,
  modelInfo,
  onLlmDone,
  onLlmError,
  onLlmReasoning,
  onLlmToken,
  onLlmToolCall,
  onLlmToolResult,
  onTerminalChunk,
  diffLineKind,
  onLlmPhase,
  onWorkspaceRoots,
  workspaceRoots,
  setWorkspaceRoots,
  onVerboseStatus,
  verboseStatus,
  phaseStatusLine,
  formatTokens,
  onHidApprovalRequest,
  onHidApprovalResolved,
  onMcpApprovalRequest,
  onMcpApprovalResolved,
  onModelInfoBroadcast,
  onRouted,
  onNudgeDismiss,
  onNudgeShow,
  onPrivacyChanged,
  onRunState,
  openCaptureSettings,
  openInputSettings,
  privacyStatus,
  requestCapturePermission,
  requestInputPermission,
  respondHidApproval,
  respondMcpApproval,
  runState,
  sendChat,
  setMemoryRetention,
  setModel,
  showStopButton,
  startHealthProbe,
  stopChat,
  stripFailedTail,
  toCaptureFlowError,
  type ApprovalVerdict,
  type McpApprovalVerdict,
} from "./chat";
import {
  hotkeyFinishesTour,
  initialTourState,
  tourFinishBlocked,
  tourOnLastStep,
  tourReducer,
  tourVisible,
  type Retention,
} from "./tour-state";
import { Tour } from "./Tour";
import { EyeIcon } from "./ui/EyeIcon";
import { Markdown } from "./ui/Markdown";
import { Toast } from "./ui/Toast";
import { ApprovalCard } from "./ui/ApprovalCard";
import {
  hideOverlay,
  onOverlayStateChanged,
  type OverlayState,
} from "./overlay-state";
import {
  centeredModalRect,
  draggedExtent,
  draggedModalSize,
  drawerRect,
  extentFromSize,
  isOnScreen,
  type DrawerRect,
  type Edge,
  type OverlayPoint,
  type OverlaySize,
  type WorkArea,
} from "./overlay-geometry";
import {
  drawerEdgeOf,
  drawerExtentFor,
  onOverlayPresentation,
  overlayPresentation,
  setOverlayExtent,
  setOverlayPosition,
  type PresentationStatus,
} from "./overlay-presentation-state";
import { onTrayNotice, type TrayNotice } from "./tray-notice";
import { listen } from "@tauri-apps/api/event";
import {
  availableMonitors,
  currentMonitor,
  getCurrentWindow,
  LogicalPosition,
  LogicalSize,
} from "@tauri-apps/api/window";

// The overlay-presentation config (M006/S04) is the authoritative geometry
// source: `overlay_presentation` on mount restores the persisted shape and the
// `overlay://presentation` broadcast pushes every live change. It replaces the
// S02/S03 `?edge=` dev harness — which is now retained ONLY as a documented
// TEST HOOK (below) that seeds the INITIAL drawer edge so Playwright can render
// the drawer/modal DOM variants without a Tauri backend. The moment the real
// config read resolves it overrides this seed, so there is no diverging runtime
// source (S04 Integration Closure: "documented if kept as a test hook").

// The default extents/size the ?edge= test seed carries. These mirror the Rust
// OverlayPresentation::default fields (config.rs); the production values always
// arrive via overlay_presentation() and supersede these.
const SEED_EDGE_EXTENTS = { top: 320, bottom: 320, left: 420, right: 420 } as const;
const SEED_MODAL_SIZE = { width: 720, height: 480 } as const;

// Apply a window rect with size FIRST, then position — SEQUENTIALLY, never
// Promise.all. On macOS tao converts the top-left y into Cocoa's bottom-left
// origin using the window's CURRENT height, and a later size change grows the
// frame from that bottom-left origin — upward. Position-then-size therefore
// pushed the top of a freshly-snapped full-height drawer above the screen: it
// rendered as a short strip stuck over the menu bar in the top corner until a
// manual resize re-applied position after size (the first-snap bug). Ordering
// size→position makes the position call see the FINAL height, every time.
// Callers must construct this inside a promise chain: getCurrentWindow()
// throws synchronously outside a Tauri runtime.
function applyWindowRect(rect: DrawerRect): Promise<void> {
  const win = getCurrentWindow();
  return win
    .setSize(new LogicalSize(rect.width, rect.height))
    .then(() => win.setPosition(new LogicalPosition(rect.x, rect.y)));
}

// TEST HOOK (not a production source): seed the initial presentation from
// ?edge= so the overlay renders a drawer variant deterministically under
// Playwright, where the Tauri invoke rejects and no config can load. An absent
// or off-contract value yields null → the default modal (floating) DOM.
function seedPresentationFromQuery(): PresentationStatus | null {
  const raw = new URLSearchParams(window.location.search).get("edge");
  const mode =
    raw === "top" || raw === "bottom" || raw === "left" || raw === "right"
      ? raw
      : null;
  if (!mode) return null;
  return {
    mode,
    edgeExtents: { ...SEED_EDGE_EXTENTS },
    modalSize: { ...SEED_MODAL_SIZE },
    modalPosition: null,
    persistError: null,
  };
}

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

  // Overlay presentation config (M006/S04): the persisted mode + per-edge
  // extents + modal size that own the overlay's geometry. Seeded from the
  // ?edge= test hook (null in production) and then made authoritative by the
  // mount read + broadcast below. The docked drawer edge is derived from it —
  // null in modal mode leaves the floating panel intact.
  const [presentation, setPresentation] = useState<PresentationStatus | null>(
    seedPresentationFromQuery,
  );
  const drawerEdge: Edge | null = presentation ? drawerEdgeOf(presentation) : null;

  // First-start tour (2026-07 redesign): the four-step wizard that replaced
  // the M006 explainer. All step/permission/retention lifecycle logic is pure
  // (tour-state.ts wrapping onboarding-state.ts); this component only fires
  // the IPC. Requesting Accessibility here does not arm HID (D038/R019).
  const [tour, dispatchTour] = useReducer(tourReducer, initialTourState);
  // The live global shortcut shown on the Summon step (null outside Tauri —
  // the step then omits the keycap row rather than inventing a binding).
  const [tourHotkey, setTourHotkey] = useState<string | null>(null);
  // The once-registered blur/Escape dismiss handler needs live tour
  // visibility without re-binding: while the tour is up we must NOT
  // dismiss on blur/Escape, or losing key focus (this app is Accessory and
  // never frontmost, so blur is easy) drops the overlay out of visible-focused
  // and the still-rendered card goes click-through — dead Grant/Continue/Skip
  // buttons. Keeping the overlay focused keeps native mouse events on.
  const onboardingVisibleRef = useRef(tourVisible(tour));
  onboardingVisibleRef.current = tourVisible(tour);
  // Live tour state for the once-registered overlay-state listener (hotkey
  // completion needs the current step without re-subscribing), plus the
  // finish effect behind a ref for the same reason.
  const tourRef = useRef(tour);
  tourRef.current = tour;
  const finishTourRef = useRef<() => void>(() => {});

  // Token deltas are coalesced per animation frame so a fast stream costs at
  // most one render per frame, not one per token. Terminal events carry the
  // authoritative full text, so a tail left in this buffer is harmless — the
  // reducer drops it as stale once the request id is cleared.
  const tokenBufferRef = useRef(new Map<number, string>());
  const reasoningBufferRef = useRef(new Map<number, string>());
  const rafRef = useRef<number | null>(null);

  useEffect(() => {
    const unlisten = onOverlayStateChanged((next) => {
      // Summon-step completion: the global hotkey's toggle is the only path
      // that hides a focused overlay while the tour holds the Escape/blur
      // guard, so `hidden` during the Summon step means "the user tried the
      // hotkey" — finish the tour exactly as the design's try-it-now promises.
      if (next === "hidden" && hotkeyFinishesTour(tourRef.current)) {
        finishTourRef.current();
      }
      setState(next);
    });
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
      // While the first-run explainer is up, blur/Escape must not tear the
      // overlay down — dismissing drops it out of visible-focused and the
      // still-rendered onboarding panel becomes click-through (dead buttons).
      if (onboardingVisibleRef.current) return;
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
      // Reasoning deltas coalesce on the same frame — they target the transient
      // Thinking… region (a different field than `text`), so flushing them after
      // tokens can't reorder anything user-visible in the answer body.
      const bufferedReasoning = reasoningBufferRef.current;
      reasoningBufferRef.current = new Map();
      for (const [requestId, delta] of bufferedReasoning) {
        dispatchChat({ type: "reasoning", payload: { requestId, delta } });
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
      onLlmReasoning((payload) => {
        const buffer = reasoningBufferRef.current;
        buffer.set(payload.requestId, (buffer.get(payload.requestId) ?? "") + payload.delta);
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
      // Live build output (coding-agent S4) streams into the terminal block.
      onTerminalChunk((payload) => dispatchChat({ type: "terminal-chunk", payload })),
      // Background-wait status pings (loading model / reading prompt).
      onLlmPhase((payload) => dispatchChat({ type: "phase", payload })),
      // Verbose-mode toggle applies live from the Settings window.
      onVerboseStatus((status) => setVerbose(status.enabled)),
      // Workspace chip stays truthful across Settings/CLI/Finder changes.
      onWorkspaceRoots((status) => setWsRoots(status.roots)),
      // Bridge v2 (spec 2026-08-02 N3): a CLI/Finder show-overlay may
      // carry a prefill for the input. (CLI `ask` runs backend-side.)
      listen<{ text: string }>("bridge://prefill", (e) => {
        setDraft(e.payload.text);
        inputRef.current?.focus();
      }),
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

  // Stick-to-bottom autoscroll: while an answer streams, follow it ONLY when
  // the user is already at (or near) the bottom. Scrolling up to reread
  // detaches the stick — onScroll records the position — and scrolling back
  // down within the threshold re-attaches it. A new message always re-sticks
  // (the user just asked; they want to see the answer start).
  const stickToBottomRef = useRef(true);
  const onMessagesScroll = () => {
    const el = messagesRef.current;
    if (!el) return;
    stickToBottomRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 48;
  };
  const messageCount = chat.messages.length;
  useEffect(() => {
    // A new turn (submit / answer placeholder) always re-sticks.
    stickToBottomRef.current = true;
    messagesRef.current?.scrollTo({ top: messagesRef.current.scrollHeight });
  }, [messageCount]);
  useEffect(() => {
    if (!stickToBottomRef.current) return;
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

  // First-run tour snapshot. Decides whether to show the wizard; outside a
  // Tauri runtime the invokes reject and the tour simply never shows (same
  // absorb posture as the queries above). The backend also shows the overlay
  // from setup() when onboarding is pending, so the card is visible without
  // the user summoning it. Retention + hotkey load alongside: both are
  // display data the wizard's later steps need, harmless if they lose the
  // race (the reducer folds them whenever they land).
  useEffect(() => {
    let cancelled = false;
    // TEST HOOK (?edge= precedent): outside a Tauri runtime first_run_status
    // rejects, so Playwright can never see the tour. `?tour=pending` seeds a
    // fresh-install snapshot (capture ungranted → hard block) and
    // `?tour=granted` a grantable one, letting e2e drive the wizard DOM. The
    // real snapshot below overrides the seed the moment it resolves, so there
    // is no diverging runtime source inside the app.
    const tourSeed = new URLSearchParams(window.location.search).get("tour");
    if (tourSeed === "pending" || tourSeed === "granted") {
      dispatchTour({
        type: "permissions",
        action: {
          type: "snapshot",
          status: {
            pending: true,
            capture: { granted: tourSeed === "granted", supported: true },
            input: { granted: false, supported: true },
            persistError: null,
          },
        },
      });
    }
    firstRunStatus().then(
      (status) => {
        if (!cancelled) dispatchTour({ type: "permissions", action: { type: "snapshot", status } });
      },
      (err) => console.debug("tour: first_run_status unavailable:", err),
    );
    memoryRetention().then(
      (status) => {
        if (!cancelled)
          dispatchTour({ type: "retention-loaded", value: status.retention as Retention });
      },
      (err) => console.debug("tour: memory_retention unavailable:", err),
    );
    hotkeyStatus().then(
      (status) => {
        if (!cancelled) setTourHotkey(status.shortcut);
      },
      (err) => console.debug("tour: hotkey_status unavailable:", err),
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
      // Approval prompts (the gate parks the action until answered; without
      // this subscription every prompt-requiring action hangs to the 120s
      // deny — the run_command "stuck running" bug).
      onHidApprovalRequest((request) => dispatchChat({ type: "hid-approval", request })),
      onMcpApprovalRequest((request) => dispatchChat({ type: "mcp-approval", request })),
      // Resolutions from ANY window (or the gate's timeout) clear the card
      // here too — answering in the hud-pill must not leave a stale prompt.
      onHidApprovalResolved((payload) =>
        dispatchChat({ type: "hid-approval-answered", approvalId: payload.approvalId }),
      ),
      onMcpApprovalResolved((payload) =>
        dispatchChat({ type: "mcp-approval-answered", approvalId: payload.approvalId }),
      ),
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

  // Presentation config (M006/S04): the overlay webview is the sole geometry
  // applier (it alone holds the setSize/setPosition/currentMonitor ACLs). Read
  // `overlay_presentation` on mount to restore the persisted shape after a
  // relaunch, then subscribe to the `overlay://presentation` broadcast so a
  // change from Settings (or a live resize) is adopted immediately. Outside a
  // Tauri runtime the invoke rejects and the seeded/default presentation holds
  // (same absorb posture as model_info) — never a crash.
  useEffect(() => {
    let cancelled = false;
    overlayPresentation().then(
      (status) => {
        if (!cancelled) setPresentation(status);
      },
      (err) => console.debug("overlay: overlay_presentation unavailable:", err),
    );
    const unlisten = onOverlayPresentation((status) => setPresentation(status));
    // MEM115: a capability/ACL denial rejects listen() inside the real app —
    // catch loudly so a dead subscription is visible, not a frozen surface.
    unlisten.catch((err) => console.error("overlay: event subscription failed:", err));
    return () => {
      cancelled = true;
      unlisten.then((f) => f());
    };
  }, []);

  // Apply the presentation geometry (M006/S02–S04): a direct window side-effect,
  // never through OverlayState/dispatch (D040/MEM148, preserving D018 click-
  // through). In a drawer mode, read the active display's WORK AREA (excludes
  // menu bar / dock / notch) via currentMonitor() and snap flush with the
  // per-edge extent (drawerRect converts physical→logical once). In modal mode,
  // restore the free-floating size (no reposition — the user drags it). Runs on
  // every presentation change, so a broadcast re-applies the persisted shape
  // idempotently. Rejections are benign (no Tauri under vite dev, or a missing
  // ACL grant) so absorb-and-log, mirroring startPanelDrag/startPanelResize.
  useEffect(() => {
    if (!presentation) return;
    const edge = drawerEdgeOf(presentation);
    if (edge) {
      const extent = drawerExtentFor(presentation, edge);
      currentMonitor()
        .then((monitor) => {
          if (!monitor) {
            // No monitor resolved (headless / detached) — nothing to snap to.
            console.debug("overlay: drawer snap skipped, no current monitor");
            return;
          }
          const rect = drawerRect(
            { ...monitor.workArea, scaleFactor: monitor.scaleFactor },
            edge,
            extent,
          );
          // applyWindowRect calls getCurrentWindow(), which reads
          // window.__TAURI_INTERNALS__.metadata synchronously — outside a Tauri
          // runtime it THROWS, so it must stay inside this .then (never eager in
          // the effect body) or a plain-browser render crashes. currentMonitor()
          // has already rejected there. Size-before-position ordering is the
          // first-snap fix (see applyWindowRect).
          return applyWindowRect(rect);
        })
        .catch((err) => console.debug("overlay: drawer snap no-op:", err));
    } else {
      // Modal (M006/S05): restore the free-floating size AND position. A stored
      // modalPosition still on-screen is restored via setPosition; absent or
      // off-screen (a point on a since-removed monitor — the OFF-SCREEN-BUT-
      // FINITE half of SC4 the Rust interpreter can't see) falls back to a
      // centered rect via centeredModalRect. Both fallbacks share that one
      // centering computation. Re-applying the same stored point on a broadcast
      // is a no-op move, so a live resize can't yank a modal the user just
      // dragged. Defer getCurrentWindow() into the promise so its synchronous
      // throw outside a Tauri runtime becomes a caught rejection.
      const size = presentation.modalSize;
      const position = presentation.modalPosition;
      Promise.all([availableMonitors(), currentMonitor()])
        .then(([monitors, monitor]) => {
          if (position && isOnScreen(position, monitors)) {
            return applyWindowRect({
              x: position.x,
              y: position.y,
              width: size.width,
              height: size.height,
            });
          }
          if (!monitor) {
            // No monitor resolved (headless / detached) — size only, no anchor
            // to center against.
            console.debug("overlay: modal center skipped, no current monitor");
            return getCurrentWindow().setSize(
              new LogicalSize(size.width, size.height),
            );
          }
          const rect = centeredModalRect(
            { ...monitor.workArea, scaleFactor: monitor.scaleFactor },
            size,
          );
          return applyWindowRect(rect);
        })
        .catch((err) => console.debug("overlay: modal geometry no-op:", err));
    }
  }, [presentation]);

  const attachScreen = () => {
    if (chatRef.current.attachPending) return;
    dispatchChat({ type: "attach-start" });
    captureScreen().then(
      (frame) => dispatchChat({ type: "attach-done", frame }),
      (err) => dispatchChat({ type: "attach-error", error: toCaptureFlowError(err) }),
    );
  };

  /** Files picked from disk: images stage like a screenshot; text files
   *  become context chips whose content rides the next message. */
  const attachFiles = (list: FileList | null) => {
    setAttachNote(null);
    for (const file of Array.from(list ?? [])) {
      if (file.type.startsWith("image/")) {
        dispatchChat({ type: "attach-start" });
        frameFromImageFile(file).then(
          (frame) => dispatchChat({ type: "attach-done", frame }),
          (err) =>
            dispatchChat({
              type: "attach-error",
              error: { kind: "capture-failed", detail: String(err) },
            }),
        );
        continue;
      }
      if (file.size > 1_000_000) {
        setAttachNote(`${file.name}: over 1 MB — attach a smaller file`);
        continue;
      }
      void file.text().then(
        (text) => {
          if (text.includes("\u0000")) {
            setAttachNote(`${file.name}: not a text file`);
            return;
          }
          setFileContexts((existing) => [
            ...existing.filter((f) => f.name !== file.name),
            { name: file.name, path: (file as { path?: string }).path ?? file.name, text },
          ]);
        },
        () => setAttachNote(`${file.name}: could not be read`),
      );
    }
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

  // Geometry (M006/S01): the header drags the whole panel via Tauri's native
  // window drag (performWindowDragWithEvent — supported on macOS, unlike
  // resize dragging); resizing is the JS drag loop below. Neither activates
  // the app or voids click-through (D018). Both affordances live inside
  // .overlay-panel, whose pointer-events is auto only in visible-focused, so
  // an idle click-through overlay is not draggable — the correct security
  // posture. Rejections here are benign (e.g. no Tauri runtime under vite
  // dev), so absorb-and-log.
  const startPanelDrag = (event: React.MouseEvent) => {
    // Only a primary-button drag on the header moves the window; let clicks on
    // interactive children (buttons) through unmolested.
    if (event.button !== 0) return;
    getCurrentWindow()
      .startDragging()
      .catch((err) => console.debug("overlay: startDragging no-op:", err));
  };

  // Pointer-driven resize (M006/S03 fix): tao's drag_resize_window is
  // NotSupported on macOS, so the native startResizeDragging this used to call
  // silently no-oped there — the drawer's inner-edge bar and the modal corner
  // grip rendered but dragging them did nothing. One JS loop now drives resize
  // on every platform: at mousedown, snapshot the window's logical size and the
  // active work area; on each mousemove, turn the pointer delta (screenX/Y —
  // stable while the window itself moves/resizes; clientX/Y would drift) into a
  // new rect via the pure dragged* helpers and apply it, latest-wins so a fast
  // drag never queues stale rects behind slow IPC. Releasing anywhere ends the
  // drag and persists via persistResizeEnd — a bar-local mouseup can't be
  // relied on once the cursor leaves the 8px affordance. Outside a Tauri
  // runtime the snapshot rejects and no listeners attach (absorb-and-log).
  // `rectFor` returning null skips that move (nothing to anchor against).
  const driveResizeDrag = (
    event: React.MouseEvent,
    rectFor: (
      start: { size: OverlaySize; workArea: WorkArea | null },
      from: OverlayPoint,
      to: OverlayPoint,
    ) => { x?: number; y?: number; width: number; height: number } | null,
  ) => {
    if (event.button !== 0) return;
    const from = { x: event.screenX, y: event.screenY };
    Promise.resolve()
      .then(() => {
        const win = getCurrentWindow();
        return Promise.all([win.innerSize(), win.scaleFactor(), currentMonitor()]);
      })
      .then(([size, scale, monitor]) => {
        const logical = size.toLogical(scale);
        const start = {
          size: { width: logical.width, height: logical.height },
          workArea: monitor
            ? { ...monitor.workArea, scaleFactor: monitor.scaleFactor }
            : null,
        };
        const win = getCurrentWindow();
        let latest: { x?: number; y?: number; width: number; height: number } | null =
          null;
        let inFlight = false;
        let released = false;
        // Persist only once the queue is drained — reading innerSize() while
        // the final setSize is still in flight would persist a stale extent,
        // and the broadcast re-apply would then visibly nudge the window.
        const maybeFinish = () => {
          if (released && !inFlight && !latest) persistResizeEnd();
        };
        const pump = () => {
          if (inFlight || !latest) return;
          const rect = latest;
          latest = null;
          inFlight = true;
          // Size before position, sequentially (applyWindowRect): position's
          // flipped-y must be computed from the FINAL height or a growing
          // drawer walks its top off-screen (the first-snap bug).
          const op =
            rect.x !== undefined && rect.y !== undefined
              ? applyWindowRect({
                  x: rect.x,
                  y: rect.y,
                  width: rect.width,
                  height: rect.height,
                })
              : win.setSize(new LogicalSize(rect.width, rect.height));
          op
            .catch((err) => console.debug("overlay: resize apply no-op:", err))
            .finally(() => {
              inFlight = false;
              pump();
              maybeFinish();
            });
        };
        const onMove = (move: MouseEvent) => {
          const rect = rectFor(start, from, { x: move.screenX, y: move.screenY });
          if (rect) {
            latest = rect;
            pump();
          }
        };
        const onUp = () => {
          window.removeEventListener("mousemove", onMove);
          window.removeEventListener("mouseup", onUp);
          released = true;
          maybeFinish();
        };
        window.addEventListener("mousemove", onMove);
        window.addEventListener("mouseup", onUp);
      })
      .catch((err) => console.debug("overlay: resize drag no-op:", err));
  };

  // Modal (floating) corner grip: anchored top-left, both axes grow with the
  // pointer; position never changes, so only setSize fires.
  const startPanelResize = (event: React.MouseEvent) => {
    driveResizeDrag(event, (start, from, to) => draggedModalSize(start.size, from, to));
  };

  // Drawer resize (M006/S03): the draggable affordance is the INNER edge bar
  // (the one facing the screen interior — CSS positions it per data-edge), and
  // only the drawer's variable axis changes. draggedExtent folds in the grow
  // direction (left→+x, right→−x, top→+y, bottom→−y) so the drag grows inward
  // instead of fighting the anchor, and drawerRect re-anchors the rect flush to
  // the docked edge — a right/bottom drawer moves its origin as it grows. The
  // extent is capped at the work-area span; with no monitor to anchor against
  // (headless/detached) the move is skipped, same posture as the snap effect.
  // The guard also narrows drawerEdge to non-null — the bar only renders in
  // drawer mode, so this never no-ops in practice, but keeps the type honest.
  const startDrawerResize = (event: React.MouseEvent) => {
    if (!drawerEdge) return;
    const edge = drawerEdge;
    driveResizeDrag(event, (start, from, to) => {
      if (!start.workArea) return null;
      const scale = start.workArea.scaleFactor;
      const span =
        edge === "left" || edge === "right"
          ? start.workArea.size.width / scale
          : start.workArea.size.height / scale;
      const extent = draggedExtent(
        edge,
        extentFromSize(edge, start.size),
        from,
        to,
        span,
      );
      return drawerRect(start.workArea, edge, extent);
    });
  };

  // Persist a live resize (M006/S04): consumes the S03 extentFromSize read-back
  // seam. Called by the drag loop's window-level mouseup once the last rect has
  // been applied — read innerSize() (PHYSICAL px), convert to logical via the
  // window scale factor (the pixels-vs-points boundary drawerRect also
  // honours), derive the mode's extent, and invoke set_overlay_extent so the new
  // shape is saved. The backend floors + persists and broadcasts the result,
  // which re-applies through the effect above — idempotent because extentFromSize
  // matches drawerRect's flooring. Never rejects the UI: a persist failure rides
  // persistError on the broadcast; a no-Tauri/ACL rejection is absorbed-and-logged.
  const persistResizeEnd = () => {
    if (!presentation) return;
    const mode = presentation.mode;
    // Defer getCurrentWindow() into the promise so its synchronous throw outside
    // a Tauri runtime becomes a caught rejection (same guard as the apply effect).
    Promise.resolve()
      .then(() => {
        const win = getCurrentWindow();
        return Promise.all([win.innerSize(), win.scaleFactor()]);
      })
      .then(([size, scale]) => {
        const logical = size.toLogical(scale);
        if (mode === "modal") {
          // Modal stores both axes as the free-floating size.
          return setOverlayExtent("modal", logical.width, logical.height);
        }
        // A drawer stores only its variable axis; extentFromSize selects + floors
        // it, and the backend reads the relevant axis from (width, height).
        const extent = extentFromSize(mode, {
          width: logical.width,
          height: logical.height,
        });
        return setOverlayExtent(mode, extent, extent);
      })
      .then((status) => {
        if (status) setPresentation(status);
      })
      .catch((err) => console.debug("overlay: persist resize no-op:", err));
  };

  // Persist a live modal move (M006/S05): mirrors persistResizeEnd on the
  // drag-handle mouseup. After native startDragging hands the window back, read
  // outerPosition() (PHYSICAL px) and convert to LOGICAL via the window scale
  // factor (the pixels-vs-points boundary drawerRect/isOnScreen also honour),
  // then invoke set_overlay_position so the landing spot is saved. The backend
  // persists + broadcasts (no floor — a legal multi-monitor origin may be
  // negative) but NEVER moves the window (the ACL split); the broadcast re-
  // applies through the effect above, a no-op move onto the same point. Only
  // meaningful in modal mode — a drawer is anchored, not dragged — so skip when
  // docked. Never rejects the UI: a persist failure rides persistError; a no-
  // Tauri/ACL rejection is absorbed-and-logged.
  const persistMoveEnd = () => {
    if (!presentation || drawerEdge) return;
    // Defer getCurrentWindow() into the promise so its synchronous throw outside
    // a Tauri runtime becomes a caught rejection (same guard as the apply effect).
    Promise.resolve()
      .then(() => {
        const win = getCurrentWindow();
        return Promise.all([win.outerPosition(), win.scaleFactor()]);
      })
      .then(([position, scale]) => {
        const logical = position.toLogical(scale);
        return setOverlayPosition(logical.x, logical.y);
      })
      .then((status) => {
        if (status) setPresentation(status);
      })
      .catch((err) => console.debug("overlay: persist move no-op:", err));
  };

  const openScreenRecordingSettings = () => {
    console.debug("capture: opening Screen Recording settings from walkthrough");
    openCaptureSettings().catch((err) =>
      console.warn("capture: open settings failed:", err),
    );
  };

  // First-start tour actions. Each permission request fires the OS prompt and
  // folds the resulting live permission back into the pure reducer (via the
  // wrapped M006 lifecycle).
  const requestCapture = () => {
    dispatchTour({ type: "permissions", action: { type: "request-start", which: "capture" } });
    requestCapturePermission().then(
      (permission) =>
        dispatchTour({
          type: "permissions",
          action: { type: "request-done", which: "capture", permission },
        }),
      (err) => {
        console.warn("tour: request_capture_permission failed:", err);
        // Leave the step in requesting? No — re-query the truthful state.
        dispatchTour({
          type: "permissions",
          action: {
            type: "request-done",
            which: "capture",
            permission: chatRef.current.capturePermission ?? { granted: false, supported: true },
          },
        });
      },
    );
  };

  const requestInput = () => {
    dispatchTour({ type: "permissions", action: { type: "request-start", which: "input" } });
    requestInputPermission().then(
      (permission) =>
        dispatchTour({
          type: "permissions",
          action: { type: "request-done", which: "input", permission },
        }),
      (err) => {
        console.warn("tour: request_input_permission failed:", err);
        dispatchTour({
          type: "permissions",
          action: {
            type: "request-done",
            which: "input",
            permission: { granted: false, supported: true },
          },
        });
      },
    );
  };

  // Retention (Memory step): optimistic dispatch, then fold whatever the
  // backend says is effective — a rejected value or persist failure lands the
  // truthful state back in the chips, never a silently-lying selection.
  const chooseRetention = (value: Retention) => {
    dispatchTour({ type: "retention", value });
    setMemoryRetention(value).then(
      (status) => {
        if (status.error) console.warn("tour: set_memory_retention:", status.error);
        dispatchTour({ type: "retention-loaded", value: status.retention as Retention });
      },
      (err) => console.debug("tour: set_memory_retention unavailable:", err),
    );
  };

  // Finish or skip the tour — both persist the done flag so the wizard never
  // shows again. The overlay stays visible (the user summoned nothing); it
  // dismisses on the next Escape/blur like any other panel. Also fired by the
  // Summon step's try-it-now hotkey press (via finishTourRef).
  const finishTour = () => {
    // Defense-in-depth: never persist "done" while a required permission is
    // missing, even if the disabled Continue button were somehow bypassed.
    // Step-independent (tourFinishBlocked): Skip from ANY step is caught, not
    // just Finish on the Permissions step.
    if (tourFinishBlocked(tourRef.current)) {
      console.debug("tour: finish blocked — Screen Recording not granted");
      return;
    }
    completeFirstRun().then(
      (status) => dispatchTour({ type: "permissions", action: { type: "completed", status } }),
      // Outside Tauri the invoke rejects — dismiss anyway so the card never
      // wedges; the flag simply wasn't persisted (harmless, re-shows next run).
      (err) => {
        console.debug("tour: complete_first_run unavailable:", err);
        dispatchTour({
          type: "permissions",
          action: {
            type: "completed",
            status: { pending: false, capture: { granted: false, supported: true }, input: { granted: false, supported: true }, persistError: null },
          },
        });
      },
    );
  };
  finishTourRef.current = finishTour;

  const openAccessibilitySettings = () => {
    console.debug("tour: opening Accessibility settings from the wizard");
    openInputSettings().catch((err) =>
      console.debug("tour: open_input_settings unavailable:", err),
    );
  };

  // New chat (computer-control I3): the current session stays stored; the
  // backend starts pointing exchanges at a fresh one, and the transcript
  // clears (environment snapshots survive the reset).
  const startNewChat = () => {
    setHistoryOpen(false);
    dispatchChat({ type: "new-chat" });
    chatNewSession().catch((err) =>
      console.debug("chat: chat_new_session unavailable:", err),
    );
  };

  // Resume (2026-07-27 spec): the History picker lists stored sessions;
  // choosing one repoints the backend session AND seeds the transcript from
  // the returned messages in one IPC round-trip.
  const [historyOpen, setHistoryOpen] = useState(false);
  const [historySessions, setHistorySessions] = useState<ChatSessionSummary[] | null>(null);
  const toggleHistory = () => {
    const next = !historyOpen;
    setHistoryOpen(next);
    if (next) {
      setHistorySessions(null);
      chatSessions(12).then(
        (sessions) => setHistorySessions(sessions),
        (err) => {
          console.debug("chat: chat_sessions unavailable:", err);
          setHistorySessions([]);
        },
      );
    }
  };
  const resumeSession = (id: number) => {
    setHistoryOpen(false);
    chatResumeSession(id).then(
      (messages) => dispatchChat({ type: "resume-chat", messages }),
      (err) => console.debug("chat: chat_resume_session failed:", err),
    );
  };

  // Answer a pending approval: deliver the verdict (fire-and-forget — the
  // backend never rejects; false just means the gate already timed out) and
  // drop the prompt either way.
  const answerHidApproval = (approvalId: number, verdict: ApprovalVerdict) => {
    respondHidApproval(approvalId, verdict).catch((err) =>
      console.warn("hid: respond_hid_approval failed:", err),
    );
    dispatchChat({ type: "hid-approval-answered", approvalId });
  };

  const answerMcpApproval = (approvalId: number, verdict: McpApprovalVerdict) => {
    respondMcpApproval(approvalId, verdict).catch((err) =>
      console.warn("mcp: respond_mcp_approval failed:", err),
    );
    dispatchChat({ type: "mcp-approval-answered", approvalId });
  };

  const [routedLane, setRoutedLane] = useState<string | null>(null);
  const [verbose, setVerbose] = useState(false);
  const [wsRoots, setWsRoots] = useState<string[]>([]);
  // Attach-context row (2026-08-02 redesign): picked text files ride the
  // next message as fenced context blocks; the menu also toggles capturing
  // the screen with every message. All transient, per-session.
  const [fileContexts, setFileContexts] = useState<FileContext[]>([]);
  const [attachMenuOpen, setAttachMenuOpen] = useState(false);
  const [autoScreen, setAutoScreen] = useState(false);
  const [attachNote, setAttachNote] = useState<string | null>(null);
  const filePickRef = useRef<HTMLInputElement>(null);
  useEffect(() => {
    verboseStatus().then(
      (status) => setVerbose(status.enabled),
      () => undefined,
    );
    workspaceRoots().then(
      (status) => setWsRoots(status.roots),
      () => undefined,
    );
    const un = onRouted((payload) => setRoutedLane(payload.lane));
    return () => {
      un.then((f) => f()).catch(() => {});
    };
  }, []);

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
    // A nudge preload grounds exactly this question when still fresh — the
    // banner may have auto-timed-out minutes ago; the submit action consumes
    // the stage reducer-side either way (stale stages are simply dropped).
    const preload = freshNudgePreload(chatRef.current.nudgePreload, Date.now());
    // Attached-file context rides the WIRE turn only; the visible bubble
    // stays the question (the chips already showed what was attached).
    const contexts = fileContexts;
    const wireQuestion = trimmed + composeContextBlocks(contexts);
    dispatchChat({ type: "submit", question: trimmed, retry });
    if (contexts.length > 0) setFileContexts([]);
    // The nudge-time screenshot (if the backend retained one) rides the
    // outgoing turn so the model can SEE what the nudge saw. Fetch failure
    // or absence degrades to text-only grounding — never blocks the send.
    const framePromise: Promise<string | null> = preload
      ? nudgeContextFrame(preload.capturedAtMs).catch(() => null)
      : Promise.resolve(null);
    // Auto-screen (attach menu toggle): capture with EVERY message unless a
    // frame is already staged; capture failure degrades to text-only.
    const autoFramePromise: Promise<string | null> =
      autoScreen && !staged
        ? captureScreen().then(
            (frame) => frame.base64Png,
            () => null,
          )
        : Promise.resolve(null);
    Promise.all([framePromise, autoFramePromise]).then(([nudgeFrame, autoFrame]) => {
      const attachments = [
        ...(staged ? [{ base64Png: staged.base64Png }] : []),
        ...(autoFrame ? [{ base64Png: autoFrame }] : []),
        ...(nudgeFrame ? [{ base64Png: nudgeFrame }] : []),
      ];
      const history = composeMessages(base, wireQuestion, attachments, preload, nudgeFrame !== null);
      sendChat(history).then(
        (requestId) => dispatchChat({ type: "request-started", requestId }),
        (err) => dispatchChat({ type: "request-failed", detail: String(err) }),
      );
    });
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

  // The first-start tour takes over the overlay while first-run is pending —
  // it renders regardless of overlay state (the backend shows the overlay
  // from setup() when onboarding is pending), ahead of the nudge/chat chrome.
  if (tourVisible(tour)) {
    return (
      <div className="overlay-root" data-state={state} data-onboarding="true">
        <Tour
          tour={tour}
          hotkeyShortcut={tourHotkey}
          onNext={() => {
            if (tourOnLastStep(tour)) finishTour();
            else dispatchTour({ type: "next" });
          }}
          onBack={() => dispatchTour({ type: "back" })}
          onSkip={finishTour}
          onGrantCapture={requestCapture}
          onGrantInput={requestInput}
          onOpenCaptureSettings={openScreenRecordingSettings}
          onOpenInputSettings={openAccessibilitySettings}
          onRetention={chooseRetention}
        />
      </div>
    );
  }

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
    <div
      className="overlay-root"
      data-state={state}
      data-edge={drawerEdge ?? undefined}
      data-empty-chat={chat.messages.length === 0 || undefined}
    >
      <div className="overlay-panel">
        {/* Header drag region (M006/S01): grabs the whole panel to move the
            window. onMouseDown starts Tauri's native drag; the app never
            activates and click-through is untouched. */}
        <div
          className="overlay-drag-handle"
          onMouseDown={startPanelDrag}
          onMouseUp={persistMoveEnd}
          aria-hidden="true"
        />
        {chat.messages.length > 0 && (
          <div className="chat-messages" ref={messagesRef} onScroll={onMessagesScroll}>
            {chat.messages.map((message, index) => (
              <div
                key={index}
                className={`chat-message chat-${message.role}`}
                data-status={message.status}
              >
                {message.role === "assistant" && message.reasoning && (
                  <div
                    className="chat-reasoning"
                    data-streaming={message.status === "streaming"}
                    title="The model's reasoning (not part of the answer)"
                  >
                    <span className="chat-reasoning-label">
                      {message.status === "streaming" ? "Thinking…" : "Thought process"}
                    </span>
                    <span className="chat-reasoning-text">{message.reasoning}</span>
                  </div>
                )}
                {message.role === "assistant" && (message.steps ?? []).length > 0 && (
                  <details className="chat-steps">
                    <summary>
                      {(message.steps ?? []).length} step
                      {(message.steps ?? []).length === 1 ? "" : "s"}
                      {(message.steps ?? []).some((s) => s.ok === false)
                        ? ` · ${(message.steps ?? []).filter((s) => s.ok === false).length} failed`
                        : ""}
                    </summary>
                    <ol className="chat-steps-list">
                      {(message.steps ?? []).map((step) => (
                        <li key={step.callId} data-ok={step.ok === null ? "pending" : step.ok}>
                          <span className="chat-step-mark" aria-hidden="true">
                            {step.ok === null ? "●" : step.ok ? "✓" : "✗"}
                          </span>
                          {step.label}
                        </li>
                      ))}
                    </ol>
                  </details>
                )}
                {message.role === "assistant" &&
                  (message.terminal ?? []).map((run) => (
                    <div key={run.callId} className="chat-terminal" data-ok={run.ok ?? undefined}>
                      <div className="chat-terminal-cmd">
                        <span className="chat-terminal-prompt" aria-hidden="true">
                          $
                        </span>
                        {run.command}
                        {run.ok === null && <span className="chat-terminal-running">running…</span>}
                        {run.ok === false && <span className="chat-terminal-failed">failed</span>}
                      </div>
                      {run.preview && <pre className="chat-terminal-out">{run.preview}</pre>}
                    </div>
                  ))}
                {message.role === "assistant" &&
                  (message.diffs ?? []).map((block) => (
                    <details key={block.callId} className="chat-diff" data-ok={block.ok ?? undefined} open>
                      <summary>
                        changes
                        {block.ok === null && <span className="chat-terminal-running">diffing…</span>}
                        {block.ok === false && <span className="chat-terminal-failed">failed</span>}
                      </summary>
                      {block.report && (
                        <pre className="chat-diff-out">
                          {block.report.split("\n").map((line, i) => (
                            <span key={i} className="chat-diff-line" data-kind={diffLineKind(line)}>
                              {line}
                              {"\n"}
                            </span>
                          ))}
                        </pre>
                      )}
                    </details>
                  ))}
                {message.role === "assistant" ? (
                  <div className="chat-text chat-markdown">
                    <Markdown text={message.text} />
                  </div>
                ) : (
                  <span className="chat-text">{message.text}</span>
                )}
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
                {message.role === "assistant" &&
                  message.status === "streaming" &&
                  chat.phase !== null && (
                    <div className="chat-phase" data-phase={chat.phase.phase}>
                      {phaseStatusLine(chat.phase, verbose)}
                    </div>
                  )}
                {message.role === "assistant" && message.status === "streaming" && (
                  <span className="chat-caret" aria-label="Answer streaming" />
                )}
                {message.role === "assistant" && message.status === "interrupted" && (
                  <span className="chat-interrupted-tag">interrupted</span>
                )}
                {message.role === "assistant" && message.status === "done" && message.usage && (
                  <span
                    className="chat-usage"
                    title={`${message.usage.promptTokens.toLocaleString()} prompt tokens in, ${message.usage.completionTokens.toLocaleString()} completion tokens out (all tool rounds summed)`}
                  >
                    ↑{formatTokens(message.usage.promptTokens)} ↓
                    {formatTokens(message.usage.completionTokens)} tok
                  </span>
                )}
              </div>
            ))}
          </div>
        )}
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
        <div className="overlay-composer">
          {chat.hidApprovals.map((request) => (
            <ApprovalCard
              key={request.approvalId}
              title="Third Eye wants to act"
              summary={request.summary}
              onAllowOnce={() => answerHidApproval(request.approvalId, "allow-once")}
              onAllowAlways={() => answerHidApproval(request.approvalId, "allow-kind")}
              onAllowForever={() => answerHidApproval(request.approvalId, "allow-always")}
              onDeny={() => answerHidApproval(request.approvalId, "deny")}
            />
          ))}
          {chat.mcpApprovals.map((request) => (
            <ApprovalCard
              key={request.approvalId}
              title={`External tool: ${request.toolName}`}
              summary={request.summary}
              onAllowOnce={() => answerMcpApproval(request.approvalId, "allow-once")}
              onAllowAlways={() => answerMcpApproval(request.approvalId, "allow-tool")}
              onDeny={() => answerMcpApproval(request.approvalId, "deny")}
            />
          ))}
          {trayNotice && (
            <div className="tray-notice" role="status">
              <Toast placement="inline">
                <span className="tray-notice-text">
                  <strong>{trayNotice.title}</strong> {trayNotice.detail}
                </span>
              </Toast>
              <button
                type="button"
                className="tray-notice-dismiss"
                aria-label="Dismiss notice"
                onClick={() => setTrayNotice(null)}
              >
                ✕
              </button>
            </div>
          )}
          {/* Context row (2026-08-02 redesign): ONE line naming everything
              grounding the next message — the working directory, staged
              screenshots, attached files, the every-message screen toggle —
              behind a single ＋ Attach menu. */}
          <div className="attach-row">
            <div className="attach-menu-anchor">
              <button
                type="button"
                className="attach-button"
                disabled={chat.attachPending}
                aria-expanded={attachMenuOpen}
                onClick={() => setAttachMenuOpen((open) => !open)}
              >
                {chat.attachPending && <span className="attach-spinner" aria-hidden="true" />}
                {chat.attachPending ? "Capturing…" : "＋ Attach"}
              </button>
              {attachMenuOpen && (
                <div className="attach-menu" role="menu">
                  {chat.capturePermission?.supported !== false && (
                    <button
                      type="button"
                      role="menuitem"
                      onClick={() => {
                        setAttachMenuOpen(false);
                        attachScreen();
                      }}
                    >
                      Screenshot now
                    </button>
                  )}
                  <button
                    type="button"
                    role="menuitem"
                    onClick={() => {
                      setAttachMenuOpen(false);
                      filePickRef.current?.click();
                    }}
                  >
                    File from disk…
                  </button>
                  {chat.capturePermission?.supported !== false && (
                    <button
                      type="button"
                      role="menuitem"
                      onClick={() => {
                        setAttachMenuOpen(false);
                        setAutoScreen((on) => !on);
                      }}
                    >
                      {autoScreen ? "✓ " : ""}Screen with every message
                    </button>
                  )}
                </div>
              )}
            </div>
            <input
              ref={filePickRef}
              type="file"
              multiple
              hidden
              onChange={(event) => {
                attachFiles(event.target.files);
                event.target.value = "";
              }}
            />
            {wsRoots.map((root, index) => (
              <span
                key={root}
                className="attach-chip attach-chip--ambient"
                data-workspace-chip={index === 0 ? true : undefined}
                title={root}
              >
                {index === 0 ? "working in " : "also "}
                {root.replace(/^\/Users\/[^/]+/, "~")}
                <button
                  type="button"
                  className="attach-chip-clear"
                  aria-label={`Stop working in ${root}`}
                  onClick={() =>
                    setWorkspaceRoots(wsRoots.filter((r) => r !== root)).then(
                      (status) => setWsRoots(status.roots),
                      () => undefined,
                    )
                  }
                >
                  ×
                </button>
              </span>
            ))}
            {chat.attachment && (
              <span className="attach-chip">
                screenshot · {chat.attachment.width}×{chat.attachment.height}
                <button
                  type="button"
                  className="attach-chip-clear"
                  aria-label="Remove screen attachment"
                  onClick={() => dispatchChat({ type: "attach-clear" })}
                >
                  ×
                </button>
              </span>
            )}
            {fileContexts.map((file) => (
              <span key={file.name} className="attach-chip" title={file.path}>
                file · {file.name}
                <button
                  type="button"
                  className="attach-chip-clear"
                  aria-label={`Remove attached file ${file.name}`}
                  onClick={() =>
                    setFileContexts((existing) => existing.filter((f) => f.name !== file.name))
                  }
                >
                  ×
                </button>
              </span>
            ))}
            {autoScreen && (
              <span className="attach-chip" title="A fresh screenshot rides every message">
                screen · every message
                <button
                  type="button"
                  className="attach-chip-clear"
                  aria-label="Stop attaching the screen to every message"
                  onClick={() => setAutoScreen(false)}
                >
                  ×
                </button>
              </span>
            )}
            {attachNote && <span className="attach-privacy-hint">{attachNote}</span>}
            {/* Privacy hint only — the menu stays live so an attempted
                capture still surfaces the typed privacy-mode error. */}
            {chat.privacy?.enabled && (
              <span className="attach-privacy-hint" title="Turn Privacy Mode off in the tray menu or settings">
                Privacy Mode on — capture blocked
              </span>
            )}
          </div>
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
          <form onSubmit={onSubmit} className="overlay-input-row">
            {/* The eye mirrors the run truthfully: amber acting while a run is
                live (Stop visible), scanning green while summoned, calm watching
                otherwise. */}
            <span className="overlay-input-eye" aria-hidden="true">
              <EyeIcon
                state={
                  showStopButton(chat)
                    ? "acting"
                    : state === "visible-focused"
                      ? "thinking"
                      : "watching"
                }
                size={34}
                stroke="#ffffff"
              />
            </span>
            <input
              ref={inputRef}
              className="overlay-input"
              type="text"
              placeholder="Ask, act, or recall anything…"
              aria-label="Overlay input"
              value={draft}
              onChange={(event) => setDraft(event.target.value)}
              onPaste={(event) => {
                // Image paste (N1, spec 2026-08-02): a pasted screenshot
                // rides the SAME attachment pipeline as the screenshot
                // button. Text pastes fall through untouched.
                const item = Array.from(event.clipboardData.items).find((i) =>
                  i.type.startsWith("image/"),
                );
                const file = item?.getAsFile();
                if (!file) return;
                event.preventDefault();
                if (chat.attachPending) return;
                dispatchChat({ type: "attach-start" });
                frameFromImageFile(file).then(
                  (frame) => dispatchChat({ type: "attach-done", frame }),
                  (err) =>
                    dispatchChat({
                      type: "attach-error",
                      error: { kind: "capture-failed", detail: String(err) },
                    }),
                );
              }}
            />
            {chat.messages.length > 0 && (
              <button
                type="button"
                className="overlay-new-chat"
                title="Start a fresh chat (this one is saved)"
                onClick={startNewChat}
              >
                ＋ New
              </button>
            )}
            <button
              type="button"
              className="overlay-new-chat overlay-history"
              title="Resume a saved chat"
              aria-expanded={historyOpen}
              onClick={toggleHistory}
            >
              ⤺ History
            </button>
            {historyOpen && (
              <div className="overlay-history-list" role="listbox" aria-label="Saved chats">
                {historySessions === null && (
                  <span className="overlay-history-empty">Loading…</span>
                )}
                {historySessions !== null && historySessions.length === 0 && (
                  <span className="overlay-history-empty">No saved chats yet</span>
                )}
                {historySessions?.map((session) => (
                  <button
                    key={session.id}
                    type="button"
                    className="overlay-history-item"
                    role="option"
                    aria-selected="false"
                    onClick={() => resumeSession(session.id)}
                  >
                    <span className="overlay-history-title">{session.title || "(untitled)"}</span>
                    <span className="overlay-history-meta">
                      {new Date(session.lastAtMs).toLocaleDateString()} · {session.messageCount}
                    </span>
                  </button>
                ))}
              </div>
            )}
            <span className="overlay-esc-chip" aria-hidden="true">
              esc
            </span>
          </form>
        </div>
        {routing && (
          <div className="model-indicator" data-lane={routing.activeLane}>
            <span className="model-indicator-model" title={routing.endpoint}>
              {/* An unpinned lane means the endpoint serves its default model. */}
              {activeModelId ?? "endpoint default model"}
            </span>
            <div className="model-lanes" role="group" aria-label="Model lane override">
              <button
                type="button"
                className="model-lane model-lane--auto"
                aria-pressed={routing.auto}
                title="Each request picks its lane (chat → thin, computer tasks → heavy, code → coder)"
                onClick={() => overrideLane("auto")}
              >
                {routing.auto && routedLane ? `auto→${routedLane}` : "auto"}
              </button>
              {routing.lanes.map((lane) => (
                <button
                  key={lane.name}
                  type="button"
                  className="model-lane"
                  aria-pressed={!routing.auto && lane.name === routing.activeLane}
                  title={lane.modelId ?? "endpoint default model"}
                  onClick={() => overrideLane(lane.name)}
                >
                  {lane.name}
                </button>
              ))}
            </div>
            {(chat.sessionTokens.promptTokens > 0 ||
              chat.sessionTokens.completionTokens > 0) && (
              <span
                className="model-session-tokens"
                title={`This session: ${chat.sessionTokens.promptTokens.toLocaleString()} prompt tokens in, ${chat.sessionTokens.completionTokens.toLocaleString()} completion tokens out`}
              >
                Σ ↑{formatTokens(chat.sessionTokens.promptTokens)} ↓
                {formatTokens(chat.sessionTokens.completionTokens)}
              </span>
            )}
            {/* Honest locality badge: only when the endpoint host actually is
                this machine — never decoration (no-fake-data rule). */}
            {isLocalEndpoint(routing.endpoint) && (
              <span className="model-on-device" title={routing.endpoint}>
                ● on-device
              </span>
            )}
          </div>
        )}
        {/* Resize affordance (M006/S01+S03): mutually exclusive by mode. In
            drawer mode the INNER edge (data-edge drives its position/cursor in
            CSS, T03) grows the drawer's variable dimension; in floating mode the
            SouthEast corner grip resizes it. Showing both would let the corner
            grip resize a full-span left/right drawer's height and fight the anchor.
            Both are children of .overlay-panel, so pointer-events tracks overlay
            state — never data-edge (MEM148) — so an idle click-through overlay is
            not resizable, preserving the security posture. */}
        {drawerEdge ? (
          <div
            className="overlay-drawer-resize-edge"
            data-edge={drawerEdge}
            onMouseDown={startDrawerResize}
            aria-hidden="true"
          />
        ) : (
          <div
            className="overlay-resize-grip"
            onMouseDown={startPanelResize}
            aria-hidden="true"
          />
        )}
      </div>
    </div>
  );
}

export default App;
