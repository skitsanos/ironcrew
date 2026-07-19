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
- a per-session lock that rejects overlapping `POST /messages` with `409`
- session metadata such as agent, timestamps, and flow id

When a session is evicted for idleness, the in-memory handle is dropped, but
the persisted conversation record remains in the configured store.

Related knobs:

| Variable | Default | Purpose |
|---|---|---|
| `IRONCREW_MAX_ACTIVE_CONVERSATIONS` | `8` | Max live chat sessions kept in memory |
| `IRONCREW_CHAT_SESSION_IDLE_SECS` | `1800` | Idle time before a chat handle is evicted |
| `IRONCREW_CONVERSATION_MAX_HISTORY` | `50` | Max retained conversation messages |
| `IRONCREW_API_MESSAGE_MAX_BYTES` | `262144` | Max bytes in one HTTP chat message |
| `IRONCREW_API_MAX_IMAGES_PER_CONVERSATION` | `16` | Max retained image references per conversation |
| `IRONCREW_API_MAX_IMAGE_BYTES_PER_CONVERSATION` | `33554432` | Max decoded image bytes retained per conversation |
| `IRONCREW_LUA_MAX_MEMORY_BYTES` | `33554432` | Allocator cap for each live conversation VM |
| `IRONCREW_MAX_EVENTS` | `1000` | Replay event count cap per event bus |
| `IRONCREW_EVENT_REPLAY_MAX_BYTES` | `4194304` | Replay event byte budget |
| `IRONCREW_EVENT_MAX_BYTES` | `262144` | Individual live/replay event cap |

---

## What actually consumes memory

For HTTP traffic, memory pressure usually comes from five places:

1. Active chat sessions
2. SSE replay buffers
3. Large tool/model outputs held in memory
4. Concurrent in-flight requests
5. Temporary idempotency-response serialization buffers

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
IRONCREW_EVENT_MAX_BYTES=131072
```

### Large outputs

Large task outputs, web responses, file reads, shell output, and tool results
can dominate memory even when chat history is small. If the deployment is cost
sensitive, lower the relevant caps from the cloud defaults.

### In-flight concurrency

`IRONCREW_MAX_ACTIVE_CONVERSATIONS` does not limit simultaneous LLM calls. Ten
active sessions can still overload CPU or provider quotas if all ten send
messages at once. Treat residency and concurrency as separate control planes.

### Idempotency response buffers

Keyed run responses are tiny, but a completed chat response can approach the
provider output limit. IronCrew serializes at most
`IRONCREW_IDEMPOTENCY_MAX_RESPONSE_BYTES` for one completion and stores at most
`IRONCREW_IDEMPOTENCY_MAX_TOTAL_RESPONSE_BYTES` across retained records. The
aggregate is database/disk payload rather than permanently resident Rust heap;
the per-response value is the relevant transient RAM ceiling. For the 1 GiB
Railway/OpenShift baseline, use 4 MiB per response and 64 MiB aggregate, and
keep the provider output cap no larger than the per-response cap if every reply
must be replayable.

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

These are conservative, unbenchmarked starting points rather than capacity
claims. Confirm peak RSS with your own prompts, images, tools, and provider
responses before raising them.

### Small instance

For `256 MiB` to `512 MiB` RAM:

```bash
IRONCREW_MAX_ACTIVE_CONVERSATIONS=2
IRONCREW_MAX_ACTIVE_RUNS=1
IRONCREW_CHAT_SESSION_IDLE_SECS=300
IRONCREW_CONVERSATION_MAX_HISTORY=20
IRONCREW_CHAT_HISTORY_MAX_BYTES=4194304
IRONCREW_API_MAX_IMAGE_BYTES_PER_CONVERSATION=4194304
IRONCREW_LUA_MAX_MEMORY_BYTES=16777216
IRONCREW_PROVIDER_MAX_RESPONSE_BYTES=4194304
IRONCREW_PROVIDER_MAX_STREAM_BYTES=4194304
IRONCREW_PROVIDER_MAX_OUTPUT_BYTES=2097152
IRONCREW_MAX_EVENTS=100
IRONCREW_EVENT_REPLAY_MAX_BYTES=524288
IRONCREW_EVENT_MAX_BYTES=65536
IRONCREW_EVENT_CHANNEL_CAPACITY=8
IRONCREW_MAX_SSE_CONNECTIONS=8
IRONCREW_DEFAULT_MAX_CONCURRENT=1
IRONCREW_MAX_CONCURRENT_TASKS=2
IRONCREW_IDEMPOTENCY_MAX_RESPONSE_BYTES=2097152
IRONCREW_IDEMPOTENCY_MAX_TOTAL_RESPONSE_BYTES=33554432
```

### Medium instance

For `1 GiB` RAM:

```bash
IRONCREW_MAX_ACTIVE_CONVERSATIONS=4
IRONCREW_MAX_ACTIVE_RUNS=2
IRONCREW_CHAT_SESSION_IDLE_SECS=600
IRONCREW_CONVERSATION_MAX_HISTORY=20
IRONCREW_CHAT_HISTORY_MAX_BYTES=8388608
IRONCREW_API_MAX_IMAGE_BYTES_PER_CONVERSATION=8388608
IRONCREW_LUA_MAX_MEMORY_BYTES=25165824
IRONCREW_PROVIDER_MAX_RESPONSE_BYTES=4194304
IRONCREW_PROVIDER_MAX_STREAM_BYTES=8388608
IRONCREW_PROVIDER_MAX_OUTPUT_BYTES=4194304
IRONCREW_MAX_EVENTS=200
IRONCREW_EVENT_REPLAY_MAX_BYTES=1048576
IRONCREW_EVENT_MAX_BYTES=131072
IRONCREW_EVENT_CHANNEL_CAPACITY=8
IRONCREW_MAX_SSE_CONNECTIONS=16
IRONCREW_DEFAULT_MAX_CONCURRENT=2
IRONCREW_MAX_CONCURRENT_TASKS=4
IRONCREW_IDEMPOTENCY_MAX_RESPONSE_BYTES=4194304
IRONCREW_IDEMPOTENCY_MAX_TOTAL_RESPONSE_BYTES=67108864
```

### Large instance

For `4 GiB+` RAM with controlled workloads:

```bash
IRONCREW_MAX_ACTIVE_CONVERSATIONS=16
IRONCREW_MAX_ACTIVE_RUNS=4
IRONCREW_CHAT_SESSION_IDLE_SECS=600
IRONCREW_CONVERSATION_MAX_HISTORY=32
IRONCREW_CHAT_HISTORY_MAX_BYTES=16777216
IRONCREW_API_MAX_IMAGE_BYTES_PER_CONVERSATION=16777216
IRONCREW_LUA_MAX_MEMORY_BYTES=33554432
IRONCREW_PROVIDER_MAX_RESPONSE_BYTES=8388608
IRONCREW_PROVIDER_MAX_STREAM_BYTES=16777216
IRONCREW_PROVIDER_MAX_OUTPUT_BYTES=8388608
IRONCREW_MAX_EVENTS=250
IRONCREW_EVENT_REPLAY_MAX_BYTES=2097152
IRONCREW_EVENT_MAX_BYTES=131072
IRONCREW_EVENT_CHANNEL_CAPACITY=16
IRONCREW_MAX_SSE_CONNECTIONS=32
IRONCREW_DEFAULT_MAX_CONCURRENT=4
IRONCREW_MAX_CONCURRENT_TASKS=8
IRONCREW_IDEMPOTENCY_MAX_RESPONSE_BYTES=8388608
IRONCREW_IDEMPOTENCY_MAX_TOTAL_RESPONSE_BYTES=268435456
```

Do not treat the large profile as validated capacity. Raise it only after a
Railway/OpenShift container soak demonstrates acceptable peak RSS and shutdown
headroom; `100` resident conversations is not a safe starting point.

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

## Deployment topology

### One HTTP instance (required today)

Use this when:

- live runs, conversations, questions, cancellation, and SSE are handled by the
  HTTP API
- you need deterministic ownership during deploys and restarts
- you can scale vertically and limit admission to fit one process

This is the supported production shape. Use PostgreSQL for durable production
records, but keep exactly one `serve` replica. On OpenShift/Kubernetes use
`replicas: 1` with the `Recreate` strategy. On Railway keep `numReplicas: 1`
and `overlapSeconds: 0` as configured in `railway.json`.

Railway Pro allows ample vertical headroom, but its account-wide resource
ceiling is not a safe application setting. Set explicit service CPU/RAM limits,
then raise IronCrew concurrency only from measured container workloads.

### Multiple HTTP instances (not yet supported)

PostgreSQL shares persistent records and run leases, but these live control
objects remain process-local:

- active run handles and cancellation
- pending human questions and answers
- conversation Lua VMs and per-session locks
- SSE broadcast/replay state

The PostgreSQL idempotency ledger does coordinate duplicate run/message claims
across processes, but it does not move any of those live objects to the pod
that receives a replay or follow-up request.

Another replica can therefore return `404` for a live object that exists in the
first process. Sticky sessions reduce routing mistakes but do not provide
ownership transfer or failover, so they are not sufficient production safety.

Do not configure an HPA or Railway replicas yet. Horizontal serving requires a
distributed live-control design or deterministic resume-on-request semantics
for every stateful endpoint.

### Storage backend guidance

| Backend | Scaling posture |
|---|---|
| JSON | Single instance only |
| SQLite | Single instance only |
| PostgreSQL | Recommended for durable production storage; does not make the HTTP control plane horizontally scalable |

---

## SSE and reverse proxies

SSE is long-lived HTTP. Proxies and load balancers need explicit tuning.

Recommendations:

- disable or raise proxy read timeouts for SSE routes
- avoid response buffering on SSE paths
- keep a single backend replica for stateful IronCrew endpoints
- prefer HTTP/1.1 or verified HTTP/2 behavior for your proxy stack

Common issues:

- proxy closes idle SSE streams too early
- buffering delays event delivery
- an accidentally configured second replica receives the reconnect and cannot
  find the process-local handle

If a future deployment deliberately introduces more than one process, both
`POST /messages` and `GET /events` must be routed to the owner; that routing
mechanism does not exist in IronCrew today.

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
- reject excess work with conservative admission limits or scale the single
  instance vertically after load testing

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
IRONCREW_MAX_ACTIVE_CONVERSATIONS=4
IRONCREW_MAX_ACTIVE_RUNS=2
IRONCREW_CHAT_SESSION_IDLE_SECS=600
IRONCREW_CONVERSATION_MAX_HISTORY=20
IRONCREW_CHAT_HISTORY_MAX_BYTES=8388608
IRONCREW_API_MAX_IMAGE_BYTES_PER_CONVERSATION=8388608
IRONCREW_LUA_MAX_MEMORY_BYTES=25165824
IRONCREW_PROVIDER_MAX_RESPONSE_BYTES=4194304
IRONCREW_PROVIDER_MAX_STREAM_BYTES=8388608
IRONCREW_MAX_EVENTS=200
IRONCREW_EVENT_REPLAY_MAX_BYTES=1048576
IRONCREW_EVENT_CHANNEL_CAPACITY=8
IRONCREW_MAX_SSE_CONNECTIONS=16
IRONCREW_DEFAULT_MAX_CONCURRENT=2
```

Then raise only after load testing your actual flows.

The main rule is simple: do not treat the default of `8` as a guaranteed safe
production value. It is only a fallback default.
