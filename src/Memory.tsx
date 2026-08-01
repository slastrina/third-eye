// Memory window webview (?view=memory): Timeline / Learned / Recall over the
// real store (memory-window-state.ts holds the pure mappings; this component
// fires the IPC). Opened from the tray panel; closes via hide_memory_window.
import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  chatSessionDelete,
  chatSessionMessages,
  chatSessions,
  chatSessionsWipe,
  memoryRetention,
  type ChatSessionMessage,
  type ChatSessionSummary,
} from "./chat";
import {
  memoryDelete,
  memoryList,
  memorySetExpiry,
  memorySetPinned,
  memorySearch,
  type MemoryRecord,
  type SearchOutcome,
} from "./memory-state";
import {
  MEMORY_TABS,
  appLabel,
  byCategory,
  categoryFacets,
  durationLabel,
  filterRecords,
  learnedRecords,
  timeLabel,
  type MemoryTab,
} from "./memory-window-state";
import { MemoryGraphView } from "./MemoryGraphView";
import { EyeIcon } from "./ui/EyeIcon";
import { Panel } from "./ui/Panel";
import "./memory-window.css";

const PAGE_SIZE = 100;

export function MemoryWindow() {
  const [tab, setTab] = useState<MemoryTab>("timeline");
  const [records, setRecords] = useState<MemoryRecord[] | null>(null);
  const [filter, setFilter] = useState("");
  const [retention, setRetention] = useState<string | null>(null);
  const [recallQuery, setRecallQuery] = useState("");
  const [recallBusy, setRecallBusy] = useState(false);
  const [recall, setRecall] = useState<SearchOutcome | null>(null);
  // Chats tab (I3): stored sessions + the opened transcript. null = the
  // list/transcript hasn't loaded (or IPC is unavailable) — honest empty.
  const [sessions, setSessions] = useState<ChatSessionSummary[] | null>(null);
  const [openSession, setOpenSession] = useState<number | null>(null);
  const [transcript, setTranscript] = useState<ChatSessionMessage[] | null>(null);
  // Two-step arm for the bulk purge — one stray click must not erase history.
  const [wipeArmed, setWipeArmed] = useState(false);
  // Category facet (memory v2): null = all categories.
  const [categoryFilter, setCategoryFilter] = useState<string | null>(null);

  // Fold one updated record back into the list (pin/expiry toggles).
  const foldRecord = (updated: MemoryRecord) =>
    setRecords((current) =>
      current?.map((r) => (r.id === updated.id ? updated : r)) ?? null,
    );

  const togglePin = (record: MemoryRecord) => {
    memorySetPinned(record.id, !(record.pinned ?? false)).then(
      foldRecord,
      (err) => console.debug("memory-window: memory_set_pinned failed:", err),
    );
  };

  const setExpiry = (record: MemoryRecord, preset: string) => {
    memorySetExpiry(record.id, preset).then(
      foldRecord,
      (err) => console.debug("memory-window: memory_set_expiry failed:", err),
    );
  };

  const deleteSession = (id: number) => {
    chatSessionDelete(id).then(
      () => setSessions((current) => current?.filter((s) => s.id !== id) ?? null),
      (err) => console.debug("memory-window: chat_session_delete failed:", err),
    );
  };

  const refresh = () => {
    memoryList(PAGE_SIZE, 0).then(
      (list) => setRecords(list),
      (err) => {
        console.debug("memory-window: memory_list unavailable:", err);
        setRecords(null);
      },
    );
  };

  useEffect(() => {
    refresh();
    memoryRetention().then(
      (status) => setRetention(status.retention),
      (err) => console.debug("memory-window: memory_retention unavailable:", err),
    );
    const onVisible = () => {
      if (document.visibilityState === "visible") refresh();
    };
    document.addEventListener("visibilitychange", onVisible);
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") close();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("visibilitychange", onVisible);
      window.removeEventListener("keydown", onKeyDown);
    };
  }, []);

  // Drag the borderless window by its header dead space; interactive
  // children (close, filter) keep their events.
  const startWindowDrag = (event: React.MouseEvent) => {
    if ((event.target as HTMLElement).closest("button, input, select, a")) return;
    try {
      getCurrentWindow()
        .startDragging()
        .catch((err) => console.debug("memory-window: drag no-op:", err));
    } catch (err) {
      console.debug("memory-window: drag unavailable:", err);
    }
  };

  const close = () => {
    invoke("hide_memory_window").catch((err) =>
      console.debug("memory-window: hide unavailable:", err),
    );
  };

  const forget = (id: number) => {
    memoryDelete(id).then(refresh, (err) =>
      console.debug("memory-window: memory_delete failed:", err),
    );
  };

  useEffect(() => {
    if (tab !== "chats") return;
    // The header filter doubles as a transcript search here: a non-empty
    // query matches across every stored message, not just titles.
    chatSessions(50, filter.trim() || undefined).then(
      (list) => setSessions(list),
      (err) => {
        console.debug("memory-window: chat_sessions unavailable:", err);
        setSessions(null);
      },
    );
  }, [tab, filter]);

  const openTranscript = (id: number) => {
    setOpenSession(id);
    setTranscript(null);
    chatSessionMessages(id).then(
      (messages) => setTranscript(messages),
      (err) => console.debug("memory-window: chat_session_messages unavailable:", err),
    );
  };

  const runRecall = () => {
    const query = recallQuery.trim();
    if (!query) return;
    setRecallBusy(true);
    memorySearch(query, 8).then(
      (outcome) => {
        setRecallBusy(false);
        setRecall(outcome);
      },
      (err) => {
        console.debug("memory-window: memory_search failed:", err);
        setRecallBusy(false);
        setRecall(null);
      },
    );
  };

  const filtered = records === null ? null : filterRecords(records, filter);
  const visible = filtered === null ? null : byCategory(filtered, categoryFilter);
  const learned = visible === null ? null : learnedRecords(visible);
  const facets = filtered === null ? [] : categoryFacets(filtered);

  return (
    <div className="memwin-root">
      <Panel variant="solid" className="memwin-card">
        <div className="memwin-header" onMouseDown={startWindowDrag}>
          <button type="button" className="memwin-close" aria-label="Close" onClick={close} />
          <EyeIcon state="watching" size={26} stroke="#ffffff" />
          <span className="memwin-title">Memory</span>
          <input
            className="memwin-filter"
            type="text"
            placeholder={tab === "chats" ? "Search chats…" : "Filter moments…"}
            aria-label="Filter moments"
            value={filter}
            onChange={(event) => setFilter(event.target.value)}
          />
          {retention !== null && (
            <span className="memwin-badge">● on-device · {retention}</span>
          )}
        </div>

        <div className="memwin-tabs" role="tablist" aria-label="Memory views">
          {MEMORY_TABS.map((entry) => (
            <button
              key={entry.id}
              type="button"
              role="tab"
              aria-selected={tab === entry.id}
              className="memwin-tab"
              data-active={tab === entry.id || undefined}
              onClick={() => setTab(entry.id)}
            >
              {entry.label}
            </button>
          ))}
        </div>

        <div className="memwin-body">
          {tab === "timeline" &&
            (visible === null ? (
              <p className="memwin-empty">Memory is unavailable outside the app.</p>
            ) : visible.length === 0 ? (
              <p className="memwin-empty">
                {filter ? "No moments match the filter." : "Nothing observed yet."}
              </p>
            ) : (
              <div className="memwin-timeline">
                {facets.length > 1 && (
                  <div className="memwin-facets" role="group" aria-label="Filter by category">
                    <button
                      type="button"
                      className="memwin-facet"
                      data-active={categoryFilter === null || undefined}
                      onClick={() => setCategoryFilter(null)}
                    >
                      all
                    </button>
                    {facets.map(({ category, count }) => (
                      <button
                        key={category}
                        type="button"
                        className="memwin-facet"
                        data-active={categoryFilter === category || undefined}
                        onClick={() =>
                          setCategoryFilter(categoryFilter === category ? null : category)
                        }
                      >
                        {category} · {count}
                      </button>
                    ))}
                  </div>
                )}
                {visible.map((record) => (
                  <div key={record.id} className="memwin-row memwin-row--stack">
                    <div className="memwin-row-main">
                      <span className="memwin-row-time">{timeLabel(record)}</span>
                      <span className="memwin-row-dot" aria-hidden="true" />
                      <span className="memwin-row-app">{appLabel(record)}</span>
                      <span className="memwin-row-text">{record.summary}</span>
                      <span className="memwin-row-dur">{durationLabel(record)}</span>
                      <button
                        type="button"
                        className="memwin-row-pin"
                        data-pinned={record.pinned || undefined}
                        title={record.pinned ? "Pinned — never expires" : "Pin (keep forever)"}
                        aria-label={`${record.pinned ? "Unpin" : "Pin"}: ${record.summary}`}
                        onClick={() => togglePin(record)}
                      >
                        {record.pinned ? "★" : "☆"}
                      </button>
                      <button
                        type="button"
                        className="memwin-row-forget"
                        title="Forget this moment"
                        aria-label={`Forget: ${record.summary}`}
                        onClick={() => forget(record.id)}
                      >
                        ✕
                      </button>
                    </div>
                    <div className="memwin-row-meta">
                      {record.category && record.category !== "other" && (
                        <span className="memwin-chip memwin-chip--category">
                          {record.category}
                        </span>
                      )}
                      {(record.tags ?? []).map((tag) => (
                        <button
                          key={tag}
                          type="button"
                          className="memwin-chip"
                          title={`Filter by “${tag}”`}
                          onClick={() => setFilter(tag)}
                        >
                          #{tag}
                        </button>
                      ))}
                      <select
                        className="memwin-expiry"
                        aria-label={`Retention for: ${record.summary}`}
                        value=""
                        onChange={(event) => {
                          const choice = event.target.value;
                          if (!choice) return;
                          if (choice === "forever") {
                            memorySetPinned(record.id, true).then(foldRecord, (err) =>
                              console.debug("memory-window: pin failed:", err),
                            );
                            return;
                          }
                          // Leaving forever: unpin first, then apply the tier.
                          const apply = () => setExpiry(record, choice);
                          if (record.pinned) {
                            memorySetPinned(record.id, false).then(apply, (err) =>
                              console.debug("memory-window: unpin failed:", err),
                            );
                          } else {
                            apply();
                          }
                        }}
                      >
                        <option value="" disabled>
                          {record.pinned
                            ? "kept forever"
                            : record.expiresAtMs
                              ? `expires ${new Date(record.expiresAtMs).toLocaleDateString()}`
                              : "standard retention"}
                        </option>
                        <option value="forever">keep forever</option>
                        <option value="7d">expire in 7 days</option>
                        <option value="30d">expire in 30 days</option>
                        <option value="60d">expire in 60 days</option>
                        <option value="standard">standard retention</option>
                      </select>
                    </div>
                  </div>
                ))}
              </div>
            ))}

          {tab === "learned" &&
            (learned === null ? (
              <p className="memwin-empty">Memory is unavailable outside the app.</p>
            ) : learned.length === 0 ? (
              <p className="memwin-empty">Nothing distilled from chats yet.</p>
            ) : (
              <div className="memwin-facts">
                {learned.map((record) => (
                  <div key={record.id} className="memwin-fact">
                    <div className="memwin-fact-text">{record.summary}</div>
                    <div className="memwin-fact-footer">
                      <span className="memwin-fact-src">
                        chat · {new Date(record.createdAtMs).toLocaleDateString()}
                      </span>
                      <button
                        type="button"
                        className="memwin-fact-forget"
                        onClick={() => forget(record.id)}
                      >
                        Forget
                      </button>
                    </div>
                  </div>
                ))}
              </div>
            ))}

          {tab === "graph" && <MemoryGraphView />}
          {tab === "chats" &&
            (openSession !== null ? (
              <div className="memwin-transcript">
                <button
                  type="button"
                  className="memwin-back"
                  onClick={() => {
                    setOpenSession(null);
                    setTranscript(null);
                  }}
                >
                  ← All chats
                </button>
                {transcript === null ? (
                  <p className="memwin-empty">Loading transcript…</p>
                ) : transcript.length === 0 ? (
                  <p className="memwin-empty">This chat has no stored messages.</p>
                ) : (
                  transcript.map((message, index) => (
                    <div
                      key={index}
                      className="memwin-bubble"
                      data-role={message.role}
                    >
                      {message.text}
                    </div>
                  ))
                )}
              </div>
            ) : sessions === null ? (
              <p className="memwin-empty">Chat history is unavailable outside the app.</p>
            ) : sessions.length === 0 ? (
              <p className="memwin-empty">
                {filter.trim() ? "No chat mentions that." : "No chats stored yet."}
              </p>
            ) : (
              <div className="memwin-timeline">
                <div className="memwin-purge-bar">
                  <button
                    type="button"
                    className="memwin-purge-all"
                    data-armed={wipeArmed || undefined}
                    onClick={() => {
                      if (!wipeArmed) {
                        setWipeArmed(true);
                        return;
                      }
                      setWipeArmed(false);
                      chatSessionsWipe().then(
                        () => setSessions([]),
                        (err) =>
                          console.debug("memory-window: chat_sessions_wipe failed:", err),
                      );
                    }}
                    onBlur={() => setWipeArmed(false)}
                  >
                    {wipeArmed
                      ? `Really delete all ${sessions.length} chats?`
                      : "Delete all chats"}
                  </button>
                </div>
                {sessions.map((session) => (
                  <div key={session.id} className="memwin-row memwin-session">
                    <button
                      type="button"
                      className="memwin-session-open"
                      onClick={() => openTranscript(session.id)}
                    >
                      <span className="memwin-row-time">
                        {new Date(session.lastAtMs).toLocaleDateString()}
                      </span>
                      <span className="memwin-row-dot" aria-hidden="true" />
                      <span className="memwin-row-text">{session.title}</span>
                      <span className="memwin-row-dur">
                        {session.messageCount} message{session.messageCount === 1 ? "" : "s"}
                      </span>
                    </button>
                    <button
                      type="button"
                      className="memwin-row-forget"
                      title="Delete this chat"
                      aria-label={`Delete chat: ${session.title}`}
                      onClick={() => deleteSession(session.id)}
                    >
                      ✕
                    </button>
                  </div>
                ))}
              </div>
            ))}

          {tab === "recall" && (
            <div className="memwin-recall">
              {recall && (
                <div className="memwin-recall-results">
                  <p className="memwin-recall-mode">
                    {recall.results.length} match{recall.results.length === 1 ? "" : "es"} ·{" "}
                    {recall.mode} ranking
                    {recall.degradeReason ? " (semantic unavailable — keyword fallback)" : ""}
                  </p>
                  {recall.results.map((record) => (
                    <div key={record.id} className="memwin-row">
                      <span className="memwin-row-time">
                        {new Date(record.spanEndMs).toLocaleDateString()}
                      </span>
                      <span className="memwin-row-dot" aria-hidden="true" />
                      <span className="memwin-row-app">{appLabel(record)}</span>
                      <span className="memwin-row-text">{record.summary}</span>
                    </div>
                  ))}
                </div>
              )}
              {recallBusy && <p className="memwin-empty">Searching…</p>}
            </div>
          )}
        </div>

        {tab === "recall" && (
          <form
            className="memwin-recall-bar"
            onSubmit={(event) => {
              event.preventDefault();
              runRecall();
            }}
          >
            <input
              className="memwin-recall-input"
              type="text"
              placeholder="Search your memory… ↵"
              aria-label="Search your memory"
              value={recallQuery}
              onChange={(event) => setRecallQuery(event.target.value)}
            />
          </form>
        )}
      </Panel>
    </div>
  );
}
