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
| `IRONCREW_MAX_CONVERSATION_LIFECYCLES` | `256` | Max distinct conversation IDs with an in-flight lifecycle operation (hard ceiling `4096`) |
| `IRONCREW_CHAT_SESSION_IDLE_SECS` | `1800` | Idle time before a chat handle is evicted |
| `IRONCREW_CONVERSATION_MAX_HISTORY` | `50` | Max retained conversation messages |
| `IRONCREW_API_MESSAGE_MAX_BYTES` | `262144` | Max bytes in one HTTP chat message |
| `IRONCREW_API_MAX_IMAGES_PER_CONVERSATION` | `16` | Max retained image references per conversation |
| `IRONCREW_API_MAX_IMAGE_BYTES_PER_CONVERSATION` | `33554432` | Max decoded image bytes retained per conversation |
| `IRONCREW_LUA_MAX_MEMORY_BYTES` | `33554432` | Allocator cap for each live conversation VM |
| `IRONCREW_MAX_EVENTS` | `1000` | Per-run replay count cap; also applies to each in-memory conversation bus |
| `IRONCREW_EVENT_REPLAY_MAX_BYTES` | `4194304` | Per-run replay byte budget; in-memory for conversations/JSON/SQLite and logical PostgreSQL journal accounting for runs |
| `IRONCREW_EVENT_MAX_BYTES` | `262144` | Individual live/durable event cap |

---

## What actually consumes memory

For HTTP traffic, memory pressure usually comes from seven places:

1. Active chat sessions
2. SSE replay buffers
3. Large tool/model outputs held in memory
4. Concurrent in-flight requests
5. Temporary idempotency-response serialization buffers
6. Per-conversation lifecycle gates
7. PostgreSQL event-journal producer queues and reader pages

### Active chat sessions

Every active conversation retains its current message history plus the live Lua
runtime needed to continue the session. If you raise
`IRONCREW_MAX_ACTIVE_CONVERSATIONS` without also tightening idle eviction and
history caps, memory usage grows linearly with session count.

### SSE replay buffers

IronCrew replays past events to late SSE subscribers. Conversation SSE and
JSON/SQLite run SSE keep that replay in memory for the life of the event bus.
PostgreSQL run HTTP replay uses a bounded database journal, but its active
EventBus still retains the normal bounded local replay alongside a bounded
producer queue; each subscriber also reads one bounded page at a time.
Chat-heavy and tool-heavy workloads can therefore produce large event payloads
in both pod RAM and PostgreSQL.

Cap replay aggressively in Cloud environments:

```bash
IRONCREW_MAX_EVENTS=200
IRONCREW_EVENT_REPLAY_MAX_BYTES=1048576
IRONCREW_EVENT_MAX_BYTES=131072
```

For PostgreSQL, also cap the transient read page and global logical retention:

```bash
IRONCREW_EVENT_JOURNAL_MAX_TOTAL_EVENTS=10000
IRONCREW_EVENT_JOURNAL_MAX_TOTAL_BYTES=67108864
IRONCREW_EVENT_JOURNAL_PAGE_MAX_BYTES=262144
IRONCREW_EVENT_JOURNAL_POLL_INTERVAL_MS=1000
IRONCREW_EVENT_JOURNAL_READ_TIMEOUT_MS=2000
IRONCREW_EVENT_JOURNAL_WRITE_TIMEOUT_MS=5000
IRONCREW_EVENT_JOURNAL_PRUNE_BATCH=500
```

The producer queue is at most 64 events and defaults to a 1 MiB byte budget,
enlarged only enough for the configured single-event maximum without exceeding
the per-run budget. Global journal bytes are logical accounting with at least
1 KiB charged per event; they exclude PostgreSQL row/page/index overhead, WAL,
replicas/backups, and dead tuples. Monitor real database storage and autovacuum
separately from pod RSS.

The write value above is the maximum supported managed-PostgreSQL setting, not
the `1500` ms application default. With `W=5000`, PostgreSQL lock/statement
timeouts are 4 seconds, three attempts plus 50/100 ms backoffs consume at most
15.15 seconds, and flush/terminal acknowledgement is bounded to 15.65 seconds.
This bounded wait preserves terminal-run progress; it does not guarantee that a
backlogged or unavailable journal is complete.

### Large outputs

Large task outputs, web responses, file reads, shell output, and tool results
can dominate memory even when chat history is small. If the deployment is cost
sensitive, lower the relevant caps from the cloud defaults.

### In-flight concurrency

`IRONCREW_MAX_ACTIVE_CONVERSATIONS` does not limit simultaneous LLM calls. Ten
active sessions can still overload CPU or provider quotas if all ten send
messages at once. Treat residency and concurrency as separate control planes.

### Lifecycle gate registry

Start, message, delete, and idle-eviction operations use a process-local gate
per `(flow, conversation ID)` so delete/recreate and message races have a clear
order. The registry retains only keys with a currently owned operation and
removes the exact key when its final owner exits; lookup and cleanup do not scan
all prior IDs.

`IRONCREW_MAX_CONVERSATION_LIFECYCLES` bounds the number of distinct keys that
can be owned concurrently (default `256`, hard ceiling `4096`). At saturation,
a request for a new distinct key receives `503`; operations already sharing an
existing key retain their normal serialization and `409` fail-fast behavior.
Set this above expected legitimate concurrent conversation mutations but below
the ingress concurrency budget. It is independent of the live-session residency
cap and does not coordinate gates across pods.

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
- `IRONCREW_MAX_CONVERSATION_LIFECYCLES` bounds distinct conversation keys with
  an operation currently in flight
- `IRONCREW_DEFAULT_MAX_CONCURRENT` controls task parallelism inside crew runs
- provider-side rate limits still apply independently
- request bursts can saturate CPU even if session count is low

In practice:

- low active count + high request bursts can still cause latency spikes
- high active count + low traffic can still waste RAM

If you need predictable latency, add an external rate limiter or gateway-level
concurrency control in front of IronCrew.

---

## Multi-replica capacity arithmetic

For `R` identical HTTP replicas, calculate the declared process-local envelope
before scaling:

| Resource | Aggregate planning formula |
|---|---|
| Active runs | `R × IRONCREW_MAX_ACTIVE_RUNS` |
| Resident conversations | `R × IRONCREW_MAX_ACTIVE_CONVERSATIONS` |
| Live run plus conversation SSE | `R × IRONCREW_MAX_SSE_CONNECTIONS` |
| Maximum PostgreSQL pool connections | `R × IRONCREW_DB_POOL_SIZE` |
| Platform memory allocation | `R × per-pod memory limit` |
| Base admitted top-level Lua allocator budget | `R × (active-run limit + active-conversation limit) × IRONCREW_LUA_MAX_MEMORY_BYTES` |

The Lua expression covers only the admitted run and conversation root VMs. It
is not RSS, a complete heap bound, or an upper bound for flows that invoke
`run_flow`/`crew:subworkflow`: every active nested sub-flow owns another
individually capped Lua VM while its parent remains resident. Nested depth and
concurrency, event buffers, Rust allocations, request bodies, provider/tool
payloads, database drivers, and shutdown overlap all require additional
headroom.
Per-principal work/control/observation buckets can expose at most `R × burst`
and `R × refill rate` when traffic actually reaches every replica; they are not
a cluster-wide quota. Durable idempotency and PostgreSQL journal-global limits
remain shared and must not be multiplied.

Use the configured enforcement scope, not the variable name, when declaring a
capacity or security boundary:

| Scope | Limits and coordination in that scope |
|---|---|
| No wider than one process/replica | Active runs, resident conversations, lifecycle keys, SSE connections, work/control/observation token buckets, PostgreSQL pool connections, per-crew task concurrency, provider-instance pacing inside each live Lua VM, live event buffers, CPU, and RAM. |
| PostgreSQL-global for one table prefix | Idempotency record, in-flight, retained-response-byte, and per-principal budgets; journal total event/byte budgets; keyed claims, ownership leases, drain fences, cancellation, and encrypted HITL coordination. |
| Trusted gateway, when required | A true cluster-wide request rate, concurrent-request budget, or provider/API quota across all replicas. IronCrew has no service-wide provider semaphore and does not advertise its process-local knobs as global. |

A gateway must authenticate callers before applying principal policy, bound its
own queues and waits, and preserve one `Idempotency-Key` for retries of the same
logical mutation. A round-robin load balancer without shared gateway state does
not turn per-process token buckets into a global limit.

Provider concurrency has no process-wide semaphore. For run phases using a
crew limit `C`, `R × active-run limit × C` is only a run-phase task-slot
planning envelope. Independent conversation turns, agent/tool delegation, and
other provider work are additional. `IRONCREW_RATE_LIMIT_MS` is likewise held
by a provider instance inside one live Lua VM, not by the service as a whole.

For two replicas using the checked-in 1 GiB baseline (`2` runs, `4`
conversations, `16` SSE connections, pool `2`, 24 MiB Lua allocator cap, and
default run concurrency `2` per replica), the declared totals are 4 active
runs, 8 resident conversations, 32 SSE connections, 4 maximum PostgreSQL
connections, 2 GiB of container memory allocation, a 288 MiB top-level
run/conversation Lua allocator budget, and 8 default run-phase task slots. A
crew using the configured hard concurrency ceiling raises the run-phase slot
envelope to 16; nested sub-flow VMs and conversation/provider work remain
additional.

These values are arithmetic, not measured Railway/OpenShift capacity. Validate
RSS, cgroup peaks, PostgreSQL load, provider limits, latency, and shutdown
headroom with the representative container soak before raising them.

Also calculate rollout overlap with the maximum simultaneous pod count `P`,
not only the steady replica count `R`. Apply every per-process row above as
`P × per-replica limit`; PostgreSQL-global logical caps still do not multiply.
For a two-replica service, an old/new Railway deployment that permits complete
overlap can briefly reach `P = 2R = 4`. An OpenShift RollingUpdate with
`replicas: 2` and `maxSurge: 1` has a controller surge envelope of
`R + 1 = 3`, but terminating pods are not counted in that limit. For one
controlled rollout, budget the physical/resource envelope conservatively as
old `R` + new `R` = 4 until every terminating pod exits; overlapping rollouts
or manual deletions require observed pod counts rather than a fixed formula.
The checked-in one-replica/Recreate baselines intentionally avoid those
horizontal envelopes.

---

## Deployment topology

For the status-labeled boundary between shared PostgreSQL coordination and
process-local live control, including the two-replica release gate, see the
[Multi-Replica Deployment Contract](multi-replica.md).

The shared PostgreSQL run journal and HITL mailbox are committed but not yet in
a published release. A published `2.22.0` image does not contain them.

### One HTTP instance (general-purpose baseline)

Use this when:

- live runs, conversation SSE, unkeyed questions, or other process-local
  control is handled by the HTTP API; keyed PostgreSQL conversation
  rehydration passed IC-008's local two-process gate and affinity-free
  OpenShift canary, but the artifact is unpublished and Railway remains unrun
- you need deterministic ownership during deploys and restarts
- you can scale vertically and limit admission to fit one process

This is the safest general-purpose production shape while execution, active
conversation turns, conversation SSE, and JSON/SQLite run SSE are owner-local.
PostgreSQL run SSE is shared and keyed conversation messages can reconstruct a
committed transcript boundary, but neither moves in-flight Lua execution. The checked-in
OpenShift/Kubernetes and Railway baselines therefore keep exactly one `serve`
replica with non-overlapping replacement.

Railway Pro allows ample vertical headroom, but its account-wide resource
ceiling is not a safe application setting. Set explicit service CPU/RAM limits,
then raise IronCrew concurrency only from measured container workloads.

### Multiple HTTP instances (bounded PostgreSQL slice)

PostgreSQL shares persistent records and run leases. It also coordinates
idempotency-keyed cancellation and, when every replica can read the same
`IRONCREW_HITL_ENCRYPTION_KEYS` key set, encrypted pending questions/answers.
Steady-state replicas use the same active id; the controlled mixed-active
overlap during key rotation follows the
[staged rotation contract](multi-replica.md#durable-keyed-run-hitl-slice).
These objects remain process-local:

- active run handles and unkeyed-run cancellation
- unkeyed/non-PostgreSQL human questions and answers
- each live conversation Lua VM and its process-local cache/locks; a keyed
  PostgreSQL message may reconstruct one from a durable boundary
- JSON/SQLite conversation and run SSE buses; PostgreSQL conversation SSE is
  unsupported

PostgreSQL's bounded plaintext run-event journal is shared: any replica can
serve retained run events, and clients reconnect with
`Last-Event-ID: <run_id>:<sequence>`. The journal can contain explicit gaps and
a synthesized terminal event can be marked incomplete; it is not an execution
checkpoint or a complete audit log.

Drain-aware replicas expose `lifecycle_state` from `/capabilities` and
`X-IronCrew-Instance-Id` on authenticated protected responses. A withdrawn
replica rejects protected `POST`/`DELETE` whose lifecycle check occurs after
withdrawal while keeping reads/SSE observable; the middleware check is the
mutation-admission linearization point, so a just-admitted transition-race
request can instead receive a generic non-cacheable `503` with numeric
`Retry-After`. The PostgreSQL exact-attempt fence lets a peer refuse
cancellation/HITL rather than acknowledge control for a draining owner. This
bounds replacement of the documented keyed-run slice, but does not transfer an
in-flight run or conversation turn. A later keyed PostgreSQL message may
reconstruct a conversation only from its last committed
incarnation/revision. Keep the first fence-introducing rollout non-overlapping
or in a maintenance window because old binaries cannot publish or honor the
fence.

The shared HITL mailbox only queues an encrypted answer for the owner to poll;
it does not move the suspended Lua VM. The PostgreSQL idempotency ledger
coordinates duplicate run/message claims. A keyed conversation follow-up can
build a new local handle after a committed turn, but no in-flight provider/tool
state moves to the receiving pod.

Another replica can therefore return `404` for a JSON/SQLite conversation or
another strictly process-local object that exists in the first process. That
claim does not apply to PostgreSQL conversation history or keyed turns:
`/start`, `/messages`, `/history`, and `DELETE` consult durable state, while
PostgreSQL conversation `/events` returns the documented `409` unsupported
boundary. Sticky sessions reduce routing mistakes for owner-local surfaces but
do not provide ownership transfer or failover.

Do not configure an HPA or Railway replicas for applications that require
conversation SSE, unkeyed controls, or execution failover. The committed
IC-008 implementation supports arbitrary routing for keyed PostgreSQL
conversation messages at committed turn boundaries, and IC-008's two-process
PostgreSQL gate plus the affinity-free OpenShift canary pass. Retain one replica
until a published release contains the behavior, on Railway until its separate
IC-008 canary passes, and whenever clients require an owner-local surface.
Execution takeover still requires a checkpoint/resume design, and shared
conversation SSE remains
unsupported.

### Storage backend guidance

| Backend | Scaling posture |
|---|---|
| JSON | Single instance only |
| SQLite | Single instance only |
| PostgreSQL | Shared bounded run SSE plus documented keyed-run controls; local two-process and affinity-free OpenShift IC-008 evidence passes keyed-conversation rehydration at committed boundaries, but Railway is unrun and no published release exists; in-flight execution stays process-owned and conversation SSE is unsupported |

---

## SSE and reverse proxies

SSE is long-lived HTTP. Proxies and load balancers need explicit tuning.

Recommendations:

- disable or raise proxy read timeouts for SSE routes
- preserve `Last-Event-ID` and do not cache or transform event streams
- honor IronCrew's `Cache-Control: no-store, no-transform` and
  `X-Accel-Buffering: no` response headers
- keep a single backend replica for conversation SSE and JSON/SQLite run SSE
- prefer HTTP/1.1 or verified HTTP/2 behavior for your proxy stack

Common issues:

- proxy closes idle SSE streams too early
- buffering delays event delivery
- a conversation or JSON/SQLite run reconnect reaches a different replica and
  cannot find the process-local handle
- a PostgreSQL run cursor is malformed/cross-run (`400`) or ahead/expired
  (`409`)

In a multi-process PostgreSQL deployment, keyed conversation `POST /messages`
can cold-rehydrate on either replica; a true revision/incarnation conflict is
reported rather than routed. Conversation `GET /events` is unsupported for
the shared store. With JSON/SQLite both surfaces remain process-local, and run
SSE is also process-local. PostgreSQL run `GET /events/{run_id}` can be served
by any replica.

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

- SSE consumers reconnect successfully, but memory or PostgreSQL storage stays
  high

Response:

- lower `IRONCREW_MAX_EVENTS`
- set `IRONCREW_EVENT_REPLAY_MAX_BYTES`
- lower the PostgreSQL journal page/global logical caps and retention when that
  backend is enabled
- inspect WAL, dead tuples, autovacuum, and indexes; logical bytes are not a
  physical database quota
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
4. Verify local and PostgreSQL `Last-Event-ID` reconnect behavior through your
   proxy, including documented gaps and `400`/`409` failures
5. Verify provider quotas and backoff behavior
6. Verify idle eviction actually reduces resident memory

Track at minimum:

- process RSS
- active conversation count
- request rate
- p95 and p99 latency for `start`, `messages`, and SSE connect
- PostgreSQL journal row/logical-byte usage, query latency/timeouts, physical
  table/index/WAL growth, and autovacuum health
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
