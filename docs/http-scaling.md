# HTTP Scaling

How to size, tune, and scale IronCrew's HTTP server for production traffic.

This guide focuses on `ironcrew serve` in long-lived deployments where chat
sessions, SSE streams, run history, and provider/tool latency all affect CPU
and RAM consumption.

---

## What `IRONCREW_MAX_ACTIVE_CONVERSATIONS` means

`IRONCREW_MAX_ACTIVE_CONVERSATIONS` is an **in-memory residency cap** for live
chat sessions. It is not a throughput limit and it is not a cap on the number
of persisted conversations in storage.

An "active conversation" is a session handle currently held in the server's
`active_conversations` map. Each active session keeps:

- a live Lua VM
- a `LuaConversationInner` with message history
- a per-session SSE `EventBus`
- a per-session lock that serializes `POST /messages`
- session metadata such as agent, timestamps, and flow id

When a session is evicted for idleness, the in-memory handle is dropped, but
the persisted conversation record remains in the configured store.

Related knobs:

| Variable | Default | Purpose |
|---|---|---|
| `IRONCREW_MAX_ACTIVE_CONVERSATIONS` | `100` | Max live chat sessions kept in memory |
| `IRONCREW_CHAT_SESSION_IDLE_SECS` | `1800` | Idle time before a chat handle is evicted |
| `IRONCREW_CONVERSATION_MAX_HISTORY` | `50` | Max retained conversation messages |
| `IRONCREW_MAX_EVENTS` | `1000` | Replay event count cap per event bus |
| `IRONCREW_EVENT_REPLAY_MAX_BYTES` | `4194304` | Replay event byte budget |

---

## What actually consumes memory

For HTTP traffic, memory pressure usually comes from four places:

1. Active chat sessions
2. SSE replay buffers
3. Large tool/model outputs held in memory
4. Concurrent in-flight requests

### Active chat sessions

Every active conversation retains its current message history plus the live Lua
runtime needed to continue the session. If you raise
`IRONCREW_MAX_ACTIVE_CONVERSATIONS` without also tightening idle eviction and
history caps, memory usage grows linearly with session count.

### SSE replay buffers

IronCrew replays past events to late SSE subscribers. This is useful for
frontend reconnects, but it means events remain resident in memory for the life
of the event bus. Chat-heavy and tool-heavy workloads can produce large event
payloads.

Cap replay aggressively in Cloud environments:

```bash
IRONCREW_MAX_EVENTS=200
IRONCREW_EVENT_REPLAY_MAX_BYTES=1048576
```

### Large outputs

Large task outputs, web responses, file reads, shell output, and tool results
can dominate memory even when chat history is small. If the deployment is cost
sensitive, lower the relevant caps from the cloud defaults.

### In-flight concurrency

`IRONCREW_MAX_ACTIVE_CONVERSATIONS` does not limit simultaneous LLM calls. Ten
active sessions can still overload CPU or provider quotas if all ten send
messages at once. Treat residency and concurrency as separate control planes.

---

## Session lifecycle

For HTTP chat, the lifecycle is:

1. `POST /flows/{flow}/conversations/{id}/start`
2. Session becomes active in memory
3. Session is visible in the store and list/history endpoints
4. `POST /messages` appends turns
5. After `IRONCREW_CHAT_SESSION_IDLE_SECS` of inactivity, the live handle is evicted
6. The persisted record remains and can be resumed later

This has two operational consequences:

- you can keep long-lived chat state in PostgreSQL/SQLite/JSON without keeping
  every session resident in RAM
- a short idle timeout is often the cheapest way to control RAM under bursty
  traffic

For Cloud deployments, `300` to `600` seconds is often a better starting point
than the default `1800`.

---

## Recommended starting points

These are conservative operational defaults, not hard limits.

### Small instance

For `256 MiB` to `512 MiB` RAM:

```bash
IRONCREW_MAX_ACTIVE_CONVERSATIONS=10
IRONCREW_CHAT_SESSION_IDLE_SECS=300
IRONCREW_CONVERSATION_MAX_HISTORY=20
IRONCREW_MAX_EVENTS=100
IRONCREW_EVENT_REPLAY_MAX_BYTES=524288
IRONCREW_DEFAULT_MAX_CONCURRENT=2
```

### Medium instance

For `1 GiB` RAM:

```bash
IRONCREW_MAX_ACTIVE_CONVERSATIONS=25
IRONCREW_CHAT_SESSION_IDLE_SECS=600
IRONCREW_CONVERSATION_MAX_HISTORY=30
IRONCREW_MAX_EVENTS=200
IRONCREW_EVENT_REPLAY_MAX_BYTES=1048576
IRONCREW_DEFAULT_MAX_CONCURRENT=5
```

### Large instance

For `4 GiB+` RAM with controlled workloads:

```bash
IRONCREW_MAX_ACTIVE_CONVERSATIONS=50
IRONCREW_CHAT_SESSION_IDLE_SECS=900
IRONCREW_CONVERSATION_MAX_HISTORY=50
IRONCREW_MAX_EVENTS=500
IRONCREW_EVENT_REPLAY_MAX_BYTES=4194304
IRONCREW_DEFAULT_MAX_CONCURRENT=10
```

I would not start at `100` active conversations unless you have already
benchmarked your actual flows, prompts, tools, and providers.

---

## Throughput vs residency

Keep these separate:

- `IRONCREW_MAX_ACTIVE_CONVERSATIONS` controls how many live chat sessions stay
  in memory
- `IRONCREW_DEFAULT_MAX_CONCURRENT` controls task parallelism inside crew runs
- provider-side rate limits still apply independently
- request bursts can saturate CPU even if session count is low

In practice:

- low active count + high request bursts can still cause latency spikes
- high active count + low traffic can still waste RAM

If you need predictable latency, add an external rate limiter or gateway-level
concurrency control in front of IronCrew.

---

## Horizontal scaling

### Single instance

Use this when:

- traffic is modest
- you can tolerate one process owning all active sessions
- JSON or SQLite is sufficient

This is the simplest deployment shape.

### Multiple instances

Use PostgreSQL if you run more than one IronCrew instance.

Important distinction:

- persisted conversation state is shared through the store
- active in-memory session handles are **not** shared across instances

That means a client that starts a conversation on instance A and sends the next
message to instance B may hit a `404` for the active session even though the
conversation exists in storage.

For production HTTP chat, you should choose one of these patterns:

1. Sticky sessions at the load balancer
2. External session router keyed by `(flow, conversation_id)`
3. Future server-side resume-on-message behavior, if IronCrew adds it

For now, sticky routing is the pragmatic choice.

### Storage backend guidance

| Backend | Scaling posture |
|---|---|
| JSON | Single instance only |
| SQLite | Single instance only |
| PostgreSQL | Required for multi-instance deployments |

---

## SSE and reverse proxies

SSE is long-lived HTTP. Proxies and load balancers need explicit tuning.

Recommendations:

- disable or raise proxy read timeouts for SSE routes
- avoid response buffering on SSE paths
- keep sticky routing for chat-related endpoints
- prefer HTTP/1.1 or verified HTTP/2 behavior for your proxy stack

Common issues:

- proxy closes idle SSE streams too early
- buffering delays event delivery
- reconnect lands on a different backend than the one holding the active handle

If the client uses both `POST /messages` and `GET /events`, route both to the
same backend instance.

---

## Failure modes to watch

### Too many idle sessions

Symptom:

- high RSS even when traffic is quiet

Response:

- lower `IRONCREW_MAX_ACTIVE_CONVERSATIONS`
- lower `IRONCREW_CHAT_SESSION_IDLE_SECS`

### Long transcripts

Symptom:

- memory growth proportional to chat age
- slow turn processing as prompts grow

Response:

- lower `IRONCREW_CONVERSATION_MAX_HISTORY`
- trim prompts or summarize older context in user flows

### Large event payloads

Symptom:

- SSE consumers reconnect successfully, but memory stays high

Response:

- lower `IRONCREW_MAX_EVENTS`
- set `IRONCREW_EVENT_REPLAY_MAX_BYTES`
- cap output-heavy events where possible

### Bursty request traffic

Symptom:

- p95/p99 latency spikes
- CPU saturation without many active sessions

Response:

- reduce internal concurrency
- add external rate limiting
- scale out behind a load balancer with PostgreSQL

---

## Capacity planning checklist

Before raising the conversation cap:

1. Measure RSS with representative chat sessions open but idle
2. Measure RSS under active turn traffic
3. Measure p95 turn latency under burst load
4. Verify SSE reconnect behavior through your proxy
5. Verify provider quotas and backoff behavior
6. Verify idle eviction actually reduces resident memory

Track at minimum:

- process RSS
- active conversation count
- request rate
- p95 and p99 latency for `start`, `messages`, and SSE connect
- provider error rate and timeout rate

---

## Practical recommendation

If you are deploying IronCrew HTTP chat in a cost-sensitive Cloud environment,
start with:

```bash
IRONCREW_MAX_ACTIVE_CONVERSATIONS=25
IRONCREW_CHAT_SESSION_IDLE_SECS=600
IRONCREW_CONVERSATION_MAX_HISTORY=30
IRONCREW_MAX_EVENTS=200
IRONCREW_EVENT_REPLAY_MAX_BYTES=1048576
```

Then raise only after load testing your actual flows.

The main rule is simple: do not treat the default of `100` as a guaranteed safe
production value. It is only a fallback default.
