import {
  useDeferredValue,
  useEffect,
  useEffectEvent,
  useRef,
  useState,
} from "react";
import "./index.css";

type AppConfig = {
  ironCrewBaseUrl: string;
  flow: string;
  defaultAgent: string;
};

/** Build an IronCrew URL scoped to the configured flow. */
function icUrl(config: AppConfig, path: string): string {
  const base = config.ironCrewBaseUrl.replace(/\/$/, "");
  return `${base}/flows/${encodeURIComponent(config.flow)}${path}`;
}

type ConversationEntry = {
  id: string;
  flow: string | null;
  agent: string;
  created_at: string;
  updated_at: string;
  turn_count: number;
  active: boolean;
};

type ConversationsResponse = {
  conversations: ConversationEntry[];
  total: number;
};

type HistoryMessage = {
  role: string;
  content?: string | null;
  tool_call_id?: string | null;
};

type HistoryResponse = {
  conversation_id: string;
  flow: string | null;
  agent: string;
  created_at: string;
  updated_at: string;
  messages: HistoryMessage[];
  turn_count: number;
};

type EventEntry = {
  name: string;
  receivedAt: string;
  payload: unknown;
};

function makeSessionId() {
  return `demo-${crypto.randomUUID().slice(0, 8)}`;
}

function formatTime(value?: string | null) {
  if (!value) {
    return "n/a";
  }

  const date = new Date(value);
  if (Number.isNaN(date.getTime())) {
    return value;
  }

  return new Intl.DateTimeFormat(undefined, {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    month: "short",
    day: "numeric",
  }).format(date);
}

async function fetchJson<T>(input: RequestInfo, init?: RequestInit): Promise<T> {
  const response = await fetch(input, init);
  const text = await response.text();
  const data = text ? JSON.parse(text) : {};

  if (!response.ok) {
    throw new Error(data.error ?? response.statusText);
  }

  return data as T;
}

export function App() {
  const [config, setConfig] = useState<AppConfig | null>(null);
  const [sessions, setSessions] = useState<ConversationEntry[]>([]);
  const [selectedSessionId, setSelectedSessionId] = useState("");
  const [draftSessionId, setDraftSessionId] = useState(makeSessionId);
  const [agent, setAgent] = useState("concierge");
  const [maxHistory, setMaxHistory] = useState("50");
  const [message, setMessage] = useState("");
  const [messages, setMessages] = useState<HistoryMessage[]>([]);
  const [events, setEvents] = useState<EventEntry[]>([]);
  const [sessionFilter, setSessionFilter] = useState("");
  const [busy, setBusy] = useState<"idle" | "starting" | "sending" | "deleting">(
    "idle",
  );
  const [status, setStatus] = useState("Waiting for IronCrew configuration.");
  const deferredFilter = useDeferredValue(sessionFilter);

  // Cancels in-flight fetches tied to the current session selection when
  // the user picks a different row, starts a new session, or deletes the
  // selected one. Prevents a stale `loadHistory` (or reactivation) from
  // completing against a session the user has already navigated away from
  // and surfaces a noisy "Conversation 'X' not found" toast.
  const selectionCtrlRef = useRef<AbortController | null>(null);
  const newSelectionSignal = () => {
    selectionCtrlRef.current?.abort();
    const ctrl = new AbortController();
    selectionCtrlRef.current = ctrl;
    return ctrl.signal;
  };
  const isAbort = (error: unknown) =>
    error instanceof DOMException && error.name === "AbortError";

  const query = deferredFilter.trim().toLowerCase();
  const filteredSessions = query
    ? sessions.filter(session =>
        `${session.id} ${session.agent}`.toLowerCase().includes(query),
      )
    : sessions;

  const refreshSessions = useEffectEvent(async (signal?: AbortSignal) => {
    if (!config) return;
    const data = await fetchJson<ConversationsResponse>(
      icUrl(config, "/conversations?limit=30"),
      { signal },
    );
    setSessions(data.conversations);
  });

  const loadHistory = useEffectEvent(async (sessionId: string, signal?: AbortSignal) => {
    if (!config) return;
    const history = await fetchJson<HistoryResponse>(
      icUrl(config, `/conversations/${encodeURIComponent(sessionId)}/history`),
      { signal },
    );
    setMessages(history.messages);
  });

  // Mount-time bootstrap. React 18 StrictMode intentionally double-invokes
  // effects in dev, so without an AbortController the browser fires
  // `/config` and `/sessions` twice. The controller cancels the first
  // request when the effect re-runs, giving clean single-request behaviour
  // in both dev and prod.
  useEffect(() => {
    const controller = new AbortController();
    void (async () => {
      try {
        const nextConfig = await fetchJson<AppConfig>("/api/config", {
          signal: controller.signal,
        });
        if (controller.signal.aborted) return;
        setConfig(nextConfig);
        setAgent(nextConfig.defaultAgent);
        setStatus(
          `Connected to ${nextConfig.flow} via ${nextConfig.ironCrewBaseUrl}.`,
        );
        // Use nextConfig directly — `refreshSessions` reads `config` from
        // state, which hasn't been committed yet inside this effect.
        const data = await fetchJson<ConversationsResponse>(
          icUrl(nextConfig, "/conversations?limit=30"),
          { signal: controller.signal },
        );
        if (!controller.signal.aborted) setSessions(data.conversations);
      } catch (error) {
        if (controller.signal.aborted) return;
        if (error instanceof DOMException && error.name === "AbortError") return;
        setStatus(String(error));
      }
    })();
    return () => controller.abort();
  }, []);

  useEffect(() => {
    if (!selectedSessionId || !config) {
      return;
    }

    let closed = false;
    const source = new EventSource(
      icUrl(config, `/conversations/${encodeURIComponent(selectedSessionId)}/events`),
    );

    const appendEvent = (name: string, payload: unknown) => {
      if (closed) {
        return;
      }

      setEvents(current =>
        [{ name, payload, receivedAt: new Date().toISOString() }, ...current].slice(0, 24),
      );
    };

    const bind = (name: string) => {
      source.addEventListener(name, event => {
        try {
          appendEvent(name, JSON.parse((event as MessageEvent).data));
        } catch {
          appendEvent(name, (event as MessageEvent).data);
        }
      });
    };

    bind("conversation_started");
    bind("conversation_turn");
    bind("conversation_thinking");
    // Sub-crew progress events — fired when the conversation's tool
    // calls delegate to a sub-flow. Shows research/analysis/writing
    // steps streaming in the event panel during the turn.
    bind("crew_started");
    bind("phase_start");
    bind("task_assigned");
    bind("task_completed");
    bind("task_failed");
    bind("task_thinking");
    bind("tool_call");
    bind("tool_result");
    source.onopen = () => appendEvent("stream", "Event stream connected.");
    source.onerror = () => {
      // EventSource re-opens the connection on transient failures. Only
      // surface the "disconnected" state when the browser has truly
      // given up (readyState === CLOSED, i.e. 2). During reconnect
      // attempts the readyState is CONNECTING (0) — no UI noise needed.
      if (source.readyState === EventSource.CLOSED) {
        appendEvent("stream", "Event stream disconnected.");
      }
    };

    return () => {
      closed = true;
      source.close();
    };
  }, [selectedSessionId, config]);

  const startSession = async (sessionId = draftSessionId) => {
    const trimmedId = sessionId.trim();
    if (!trimmedId) {
      setStatus("Session id is required.");
      return;
    }
    if (!config) {
      setStatus("Waiting for config...");
      return;
    }

    setBusy("starting");
    try {
      const body: Record<string, unknown> = { agent };
      const mh = Number(maxHistory);
      if (Number.isFinite(mh) && mh >= 0) body.max_history = mh;

      await fetchJson(
        icUrl(config, `/conversations/${encodeURIComponent(trimmedId)}/start`),
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(body),
        },
      );
      setSelectedSessionId(trimmedId);
      setDraftSessionId(trimmedId);
      setEvents([]);
      await loadHistory(trimmedId);
      await refreshSessions();
      setStatus(`Session ${trimmedId} is ready.`);
    } catch (error) {
      setStatus(String(error));
    } finally {
      setBusy("idle");
    }
  };

  const sendMessage = async () => {
    const target = selectedSessionId || draftSessionId;
    const content = message.trim();
    if (!target) {
      setStatus("Start or resume a session first.");
      return;
    }
    if (!content) {
      return;
    }

    if (!config) {
      setStatus("Waiting for config...");
      return;
    }

    // Optimistically append the user message so it's visible regardless
    // of whether the server returns a reply. On error we append an
    // "error" bubble in place of the assistant turn so the failure is
    // visible inline instead of hiding in the status bar at the bottom.
    setMessages(current => [...current, { role: "user", content }]);
    setMessage("");

    setBusy("sending");
    setStatus(`Sending message to ${target}.`);
    try {
      const response = await fetchJson<{
        assistant: string;
        turn_count: number;
      }>(
        icUrl(config, `/conversations/${encodeURIComponent(target)}/messages`),
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ content }),
        },
      );
      setMessages(current => [
        ...current,
        { role: "assistant", content: response.assistant },
      ]);
      await refreshSessions();
      setStatus(`Turn ${response.turn_count} completed.`);
    } catch (error) {
      const msg = String(error);
      setMessages(current => [
        ...current,
        { role: "error", content: `⚠️ ${msg}` },
      ]);
      setStatus(msg);
    } finally {
      setBusy("idle");
    }
  };

  const deleteSession = async () => {
    if (!selectedSessionId) {
      return;
    }
    if (!config) {
      setStatus("Waiting for config...");
      return;
    }

    // Cancel any in-flight loads targeting the session we're about to
    // delete — otherwise a pending history fetch returns 404 after the
    // delete succeeds and surfaces a misleading error toast.
    selectionCtrlRef.current?.abort();

    setBusy("deleting");
    try {
      await fetchJson(
        icUrl(config, `/conversations/${encodeURIComponent(selectedSessionId)}`),
        { method: "DELETE" },
      );
      setStatus(`Deleted ${selectedSessionId}.`);
      setMessages([]);
      setEvents([]);
      setDraftSessionId(makeSessionId());
      setSelectedSessionId("");
      await refreshSessions();
    } catch (error) {
      setStatus(String(error));
    } finally {
      setBusy("idle");
    }
  };

  return (
    <div className="app-shell">
      <aside className="sidebar">
        <div className="hero-card">
          <p className="eyebrow">Bun + React showcase</p>
          <h1>chat-http operator console</h1>
          <p className="lede">
            This UI talks to the IronCrew <code>chat-http</code> flow through
            Bun proxy routes, so you can demo explicit start, message turns,
            event streaming, history, list, and delete from one screen.
          </p>
          <dl className="config-grid">
            <div>
              <dt>Flow</dt>
              <dd>{config?.flow ?? "loading"}</dd>
            </div>
            <div>
              <dt>Base URL</dt>
              <dd>{config?.ironCrewBaseUrl ?? "loading"}</dd>
            </div>
            <div>
              <dt>Default agent</dt>
              <dd>{config?.defaultAgent ?? "loading"}</dd>
            </div>
            <div>
              <dt>Auth token</dt>
              <dd>{config?.authConfigured ? "configured" : "not configured"}</dd>
            </div>
          </dl>
        </div>

        <section className="panel">
          <div className="panel-header">
            <h2>Session controls</h2>
            <button
              type="button"
              className="ghost-button"
              onClick={() => setDraftSessionId(makeSessionId())}
            >
              New id
            </button>
          </div>

          <label className="field">
            <span>Session id</span>
            <input
              value={draftSessionId}
              onChange={event => setDraftSessionId(event.target.value)}
              placeholder="demo-1234abcd"
            />
          </label>

          <div className="field-row">
            <label className="field">
              <span>Agent</span>
              <input
                value={agent}
                onChange={event => setAgent(event.target.value)}
                placeholder="concierge"
              />
            </label>

            <label className="field">
              <span>Max history</span>
              <input
                value={maxHistory}
                onChange={event => setMaxHistory(event.target.value)}
                inputMode="numeric"
              />
            </label>
          </div>

          <div className="action-row">
            <button
              type="button"
              className="primary-button"
              disabled={busy !== "idle"}
              onClick={() => void startSession()}
            >
              {busy === "starting" ? "Starting..." : "Start / Resume"}
            </button>
            <button
              type="button"
              className="ghost-button"
              disabled={busy !== "idle" || !selectedSessionId}
              onClick={() => void deleteSession()}
            >
              Delete
            </button>
          </div>
        </section>

        <section className="panel">
          <div className="panel-header">
            <h2>Stored sessions</h2>
            <button
              type="button"
              className="ghost-button"
              onClick={() => void refreshSessions()}
            >
              Refresh
            </button>
          </div>

          <label className="field">
            <span>Filter</span>
            <input
              value={sessionFilter}
              onChange={event => setSessionFilter(event.target.value)}
              placeholder="Find by id or agent"
            />
          </label>

          <div className="session-list">
            {filteredSessions.length === 0 ? (
              <p className="empty-state">No sessions visible for this flow.</p>
            ) : (
              filteredSessions.map(session => (
                <button
                  type="button"
                  key={session.id}
                  className={
                    session.id === selectedSessionId
                      ? "session-row active"
                      : "session-row"
                  }
                  onClick={() => {
                    setSelectedSessionId(session.id);
                    setDraftSessionId(session.id);
                    setStatus(`Loaded metadata for ${session.id}.`);
                    const signal = newSelectionSignal();
                    void (async () => {
                      try {
                        if (!config) return;
                        if (!session.active) {
                          // Empty body — the server resumes the session
                          // using the persisted agent.
                          await fetchJson(
                            icUrl(
                              config,
                              `/conversations/${encodeURIComponent(session.id)}/start`,
                            ),
                            {
                              method: "POST",
                              headers: { "Content-Type": "application/json" },
                              body: "{}",
                              signal,
                            },
                          );
                        }
                        await loadHistory(session.id, signal);
                      } catch (error) {
                        // Abort fires when the user changes selection or
                        // deletes this session mid-load — not a real error.
                        if (isAbort(error)) return;
                        setStatus(String(error));
                      }
                    })();
                  }}
                >
                  <div className="session-row-top">
                    <strong>{session.id}</strong>
                    <span className={session.active ? "live-pill" : "idle-pill"}>
                      {session.active ? "active" : "stored"}
                    </span>
                  </div>
                  <p>{session.agent}</p>
                  <small>
                    {session.turn_count} turn(s) · updated {formatTime(session.updated_at)}
                  </small>
                </button>
              ))
            )}
          </div>
        </section>
      </aside>

      <main className="workspace">
        <section className="transcript-panel">
          <div className="panel-header">
            <div>
              <p className="eyebrow">Conversation</p>
              <h2>{selectedSessionId || "No session selected"}</h2>
            </div>
            <div className="status-chip">{status}</div>
          </div>

          <div className="transcript">
            {messages.length === 0 ? (
              <div className="empty-transcript">
                <p>Start a session or select one from the list.</p>
                <p>
                  The first successful <code>start</code> makes the transcript
                  immediately visible through history and list endpoints.
                </p>
              </div>
            ) : (
              messages.map((entry, index) => (
                <article
                  key={`${entry.role}-${index}-${entry.content ?? ""}`}
                  className={`message-card role-${entry.role}`}
                >
                  <header>{entry.role}</header>
                  <p>{entry.content ?? "(no content)"}</p>
                </article>
              ))
            )}
          </div>

          <div className="composer">
            <textarea
              value={message}
              onChange={event => setMessage(event.target.value)}
              placeholder="Type a message for the concierge agent"
            />
            <div className="composer-actions">
              <p>
                Sends to <code>{`{flow}/conversations/:id/messages`}</code>
              </p>
              <button
                type="button"
                className="primary-button"
                disabled={busy !== "idle" || !selectedSessionId}
                onClick={() => void sendMessage()}
              >
                {busy === "sending" ? "Sending..." : "Send message"}
              </button>
            </div>
          </div>
        </section>

        <section className="events-panel panel">
          <div className="panel-header">
            <div>
              <p className="eyebrow">Live event stream</p>
              <h2>SSE replay and turn events</h2>
            </div>
          </div>

          <div className="event-list">
            {events.length === 0 ? (
              <p className="empty-state">
                Select a live session to subscribe to
                <code> /events</code>.
              </p>
            ) : (
              events.map((entry, index) => (
                <article key={`${entry.name}-${entry.receivedAt}-${index}`} className="event-card">
                  <div className="event-meta">
                    <strong>{entry.name}</strong>
                    <span>{formatTime(entry.receivedAt)}</span>
                  </div>
                  <pre>{JSON.stringify(entry.payload, null, 2)}</pre>
                </article>
              ))
            )}
          </div>
        </section>
      </main>
    </div>
  );
}

export default App;
