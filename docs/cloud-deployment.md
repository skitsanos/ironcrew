# Cloud Deployment

How to run IronCrew in managed cloud environments: **Kubernetes**, **OpenShift**, **Railway**, and similar platforms. This doc covers graceful shutdown, resource limits, security posture, and platform-specific recipes.

IronCrew is distributed as a single Rust executable. The default Linux release
uses the GNU target and is dynamically linked against glibc. The source-build
container uses Debian; the tag-owned release-image recipe uses a pinned Wolfi
base index with the required glibc, OpenSSL, and CA runtime. IronCrew runs in
`serve` mode as a long-lived HTTP server, or in `run` mode as a one-shot job.

For HTTP-specific capacity planning, active conversation sizing, SSE tuning,
and RAM limits, see [HTTP Scaling](http-scaling.md). For the exact shared-state
and live-control boundary, Railway/OpenShift routing constraints, and the
multi-replica roadmap, see the
[Multi-Replica Deployment Contract](multi-replica.md).

The PostgreSQL cross-replica HITL mailbox and run-event journal described here
are committed but not yet in a published release. Until the next release,
deploy a verified source commit rather than assuming the published `2.22.0`
image contains those capabilities.

---

## Binary profile

- Single application executable; default Linux artifacts require a compatible glibc runtime.
- Release build strips symbols and enables LTO — typical size is 15–25 MB.
- The source container uses `debian:13-slim`; the release-image recipe uses a
  content-addressed Wolfi base index and performs no moving package install.
- The image defaults to numeric UID `10001` and group `0`; writable directories are group-writable so OpenShift can substitute its namespace-assigned UID.
- The image has a runnable `CMD` (`ironcrew serve --flows-dir /flows`) and listens on port `3000` unless an environment port overrides it.
- No systemd, no daemonization — runs in the foreground; logs to stderr.

---

## Graceful shutdown

IronCrew uses a monotonic process lifecycle:

```text
accepting -> fencing -> draining -> stopping
```

On Unix, `SIGUSR1` is an explicit drain signal and does not exit the process:

1. The replica enters `fencing`. Readiness fails. A direct protected
   `POST`/`DELETE` whose lifecycle middleware check occurs after that transition
   returns a non-cacheable `503 instance_draining` with `Retry-After: 1`.
2. PostgreSQL atomically marks each exact in-flight idempotency attempt owned
   by this process as draining. A peer then returns `503 run_owner_draining`
   with `Retry-After: 1` for cancellation or HITL answer delivery instead of
   reporting false acceptance.
3. The replica enters `draining`. Already accepted execution, protected
   `GET`/`HEAD`, metrics, question reads, and new or existing SSE remain
   observable. The process stays in this state until terminated.

If the explicit PostgreSQL fence errors or times out, the lifecycle remains
`fencing`: readiness and mutations stay closed, but the process does not claim
that the durable fence exists. A later `SIGUSR1` retries it; `SIGTERM`/Ctrl+C
switches to the termination retry policy below.

`SIGTERM` (Kubernetes/Railway) and Ctrl+C start a routing deadline of
`IRONCREW_SHUTDOWN_ROUTING_GRACE_SECS` and perform the same fence. If a fence
attempt fails, IronCrew stays unready in `fencing` and retries with bounded
store attempts and exponential backoff from 100 ms, capped at 5 seconds, until
the transaction commits; it never advances to `stopping` with an unpublished
owner fence. After a successful fence, it waits any remainder of the routing
interval and then enters `stopping`:

1. Active run work is aborted. Each run monitor persists an `aborted` terminal
   state and emits its terminal event; shutdown waits for that acknowledgement
   before dropping the run handle/EventBus.
2. Active chat turns are cancelled, revision-guarded state is persisted, and
   conversation SSE closes.
3. Axum's graceful-shutdown path finishes other in-flight HTTP work. The
   `IRONCREW_SHUTDOWN_TIMEOUT_SECS` hard deadline starts at `stopping`, not when
   the routing grace begins.
4. Per-request `LuaCrew` instances drop and MCP managers start bounded cleanup.
5. `IRONCREW_SHUTDOWN_DRAIN_MS` gives Drop-spawned background cleanup a final
   window before the Tokio runtime exits.

This sequence is an owner fence and bounded stop, not execution migration.
`SIGKILL`, node loss, or a grace-period overrun still relies on lease expiry
and `abandoned` reconciliation; external tool/provider effects remain the
tool's idempotency responsibility.

The lifecycle middleware snapshot is the mutation-admission linearization
point. A request admitted while the replica was still `accepting` remains a
pre-fence request even if an inner race check later rejects it; that rejection
is a generic non-cacheable `503` with numeric `Retry-After`, not necessarily
the structured `instance_draining` body.

### Shutdown tunables

| Variable | Default | Description |
|---|---|---|
| `IRONCREW_SHUTDOWN_ROUTING_GRACE_SECS` | `5` | Routing deadline measured from SIGTERM/Ctrl+C (range `0..300`). Fencing consumes part of this interval; after a successful fence, any remainder is spent in `draining`. Fence failure retries beyond the deadline and prevents `stopping`. Use `0` only when an external drain has already been verified and the store fence is expected to commit immediately. |
| `IRONCREW_SHUTDOWN_TIMEOUT_SECS` | `10` | Hard teardown deadline in seconds, started when the process enters `stopping` (range `1..300`). The process exits if graceful teardown has not completed. |
| `IRONCREW_SHUTDOWN_DRAIN_MS` | `1000` | Milliseconds to wait after Axum returns, so Drop-spawned shutdown tasks can complete. Set to `0` to skip (children will be killed when the runtime drops). |

**Tune these values** to fit your platform's grace period:

- **Kubernetes `terminationGracePeriodSeconds: 30`** (default) → the defaults
  require at least `5 + 10 + 1` seconds before an operator safety margin.
- **Tight grace periods (≤ 10 s)** → lower the routing and cleanup windows only
  after testing actual EndpointSlice/ingress withdrawal and child cleanup.
- **Heavy MCP stdio usage** (many long-lived child processes per request) → bump to `2000–3000` to ensure every `uvx` / `npx` child exits cleanly.
- **Railway** → the platform default draining time is zero. The checked-in
  `railway.json` explicitly grants 30 seconds, so fit routing grace, stopping
  timeout, cleanup, and margin inside that value.

### Pod termination sequence (Kubernetes)

```
    kubelet              ironcrew pod
       │                      │
       │─── SIGTERM ─────────►│
       │                      │── fencing: fail readiness + reject mutations
       │                      │── fence exact owned keyed attempts in Postgres
       │                      │── draining: keep reads/SSE observable
       │                      │── remainder of SHUTDOWN_ROUTING_GRACE_SECS
       │                      │── stopping: start hard-deadline clock
       │                      │       (IRONCREW_SHUTDOWN_TIMEOUT_SECS)
       │                      │── abort active_runs
       │                      │       → persist aborted terminal states
       │                      │       → emit terminal events and await monitors
       │                      │── drop run + conversation EventBuses
       │                      │       → SSE subscribers unblock
       │                      │── axum graceful-shutdown: finish
       │                      │       remaining non-SSE in-flight requests
       │                      │── drop LuaCrews, shutdown MCP clients
       │                      │       (spawns stdio child reapers)
       │                      │── IRONCREW_SHUTDOWN_DRAIN_MS wait
       │                      │── exit 0  (or exit anyway on hard-deadline
       │                      │            if graceful shutdown overran)
       │◄── container exit ───│
       │
       │ (if still running after terminationGracePeriodSeconds)
       │─── SIGKILL ─────────►│
```

Ensure
`terminationGracePeriodSeconds >= IRONCREW_SHUTDOWN_ROUTING_GRACE_SECS + IRONCREW_SHUTDOWN_TIMEOUT_SECS + IRONCREW_SHUTDOWN_DRAIN_MS/1000 + operator margin`.
The checked-in OpenShift values use `5 + 30 + 1 + 5 = 41` seconds inside a
45-second pod grace when the durable owner fence commits inside the five-second
routing grace. A `preStop` hook consumes the same pod grace period and must be
added to the left side of that arithmetic; it does not create another
independent budget. If PostgreSQL fencing cannot commit, IronCrew deliberately
stays in `fencing` until the platform may send `SIGKILL`; it cannot promise
clean teardown or the formula in that failure mode. Lease expiry then
reconciles unfinished work as `abandoned`. Because long-running runs are
aborted after the routing grace, do **not** add `IRONCREW_MAX_RUN_LIFETIME` to
the healthy termination budget.

---

## Security posture

Production deployments should set these at minimum:

| Variable | Recommended | Why |
|---|---|---|
| `IRONCREW_API_TOKEN` | strong random visible-ASCII string (32+ bytes) | Backwards-compatible single credential protecting the API and `/metrics`; health endpoints stay public. |
| `IRONCREW_API_PRINCIPAL` | stable service/client label | Audit and quota identity for `IRONCREW_API_TOKEN`; defaults to `default`. This is operator configuration, not a caller header. |
| `IRONCREW_API_TOKENS` | secret JSON object of `principal: token` entries | Prefer for multiple callers so admission and durable idempotency quotas isolate each trusted principal. Keep it in a Secret; do not put it directly in a manifest. |
| `IRONCREW_ALLOW_UNAUTHENTICATED` | **unset** | Public binds fail closed without a token. This explicit override is for isolated development only. |
| `IRONCREW_CORS_ORIGINS` | explicit domain list | Default is **deny-all**. Set `https://app.example.com` (comma-separated for multiple). Avoid `*`. |
| `IRONCREW_ALLOW_SHELL` | **unset** | Leaving shell disabled prevents agents from running arbitrary commands. Only enable in sandboxed workloads. |
| `IRONCREW_ALLOW_PRIVATE_IPS` | **unset** | Keep SSRF protection on. Protected HTTP clients check DNS answers, actual connection addresses, and redirects, and ignore environment proxies. |
| `IRONCREW_MCP_ALLOWED_COMMANDS` | comma-separated allowlist | If using MCP stdio, whitelist only exact commands you trust (e.g. `uvx,npx`). Unset = development allow-all; present-but-empty refuses all. |
| `IRONCREW_MCP_ALLOWED_HTTP_HOSTS` | `__disabled__` or exact hosts | Exact hosts allowed for bounded HTTP MCP messages. Keep disabled unless every host is operator-trusted; bounds do not make remote side effects safe. |
| `IRONCREW_MCP_ALLOW_LOCALHOST` | **unset** | Only enable if MCP servers run as sidecars. |
| `IRONCREW_MCP_DISCOVERY_TIMEOUT_SECS` | `10` | Deadline for strict MCP `2026-07-28` `server/discover`; legacy initialize/SSE endpoints are not supported. |
| `IRONCREW_MCP_MAX_MRTR_ROUNDS` | `10` or lower | Total wire-attempt cap for state-only MRTR tool calls; hard ceiling `32`. |
| `IRONCREW_MCP_MAX_REQUEST_STATE_BYTES` | `65536` or lower | Byte cap on opaque state echoed during MRTR; hard ceiling `1048576`. |
| `IRONCREW_MCP_MAX_INBOUND_MESSAGE_BYTES` | `1048576` or lower | Pre-JSON cap per stdio line, HTTP JSON message, or SSE event; hard ceiling `16777216`. One transport chunk may temporarily exceed the cap but is rejected before copying into IronCrew-owned assembly/parser buffers. |
| `IRONCREW_MAX_BODY_SIZE` | `10485760` (10 MB) or lower | Caps request body size against memory-exhaustion DoS. |
| `IRONCREW_HTTP_MAX_RESPONSE_BYTES` | `8388608` (8 MiB) or lower | Caps `http_request` and Lua `http.*` bodies. `IRONCREW_MAX_RESPONSE_SIZE` is only a deprecated fallback. |
| `IRONCREW_HITL_ENCRYPTION_KEYS` | secret JSON keyring, identical in steady state | Enables encrypted PostgreSQL cross-replica HITL for idempotency-keyed runs. During the controlled rotation overlap, every process must contain both keys even while active ids temporarily differ. Store only in Railway/OpenShift secrets; never bake it into the image. |
| `IRONCREW_HITL_ACTIVE_KEY_ID` | one id from the HITL keyring | Selects the key for newly registered question metadata. Answers inherit their authenticated question's key. Both HITL variables must be set together. |
| `IRONCREW_ENV_ALLOWLIST` | comma-separated names | Fail-closed allowlist shared by Lua `env()` and `${env.NAME}` interpolation. Opt in only the exact vars a crew needs. See [docs/sandbox.md](sandbox.md). |
| `IRONCREW_TRUST_PROXY` | unset | Set to `1` only when running behind a trusted reverse proxy. Audit-log source-IP capture then prefers `X-Forwarded-For` over the direct TCP peer. Leave unset for direct-exposure deployments to prevent IP spoofing. |
| `IRONCREW_AUDIT_DEFAULT_LIMIT` | `50` | Default `GET /audit?limit=` value. |
| `IRONCREW_AUDIT_MAX_LIMIT` | `500` | Hard cap on `GET /audit?limit=`. |

Protected outbound clients deliberately ignore `HTTP_PROXY`, `HTTPS_PROXY`,
and related environment proxy settings because proxy routing would bypass the
connect-time address policy. Railway/OpenShift deployments must allow direct
egress to provider/tool destinations; a mandatory egress proxy is not currently
a supported transport path.

Bearer authentication is parsed once during process startup. Each successful
credential produces an internal principal identity; caller-provided audit
headers cannot select another principal or consume another principal's
idempotency recovery boundary. When migrating from one shared token, configure
`IRONCREW_API_TOKENS` in the platform secret, keep the legacy credential path
available for retries whose idempotency keys were created before the upgrade,
and remove it only after every caller has moved and the longest retained
idempotency TTL has elapsed. Moving a caller from the legacy credential to a
named principal intentionally changes its idempotency namespace: an old key
sent under the new identity is a different operation.

Principals provide identity, rate/quota isolation, recovery binding, and token
rotation; they are not an authorization role system. Every configured token can
reach the same protected API routes, so treat a monitoring-only token as a full
service credential and constrain its network/source access externally. Rotate
a token under the same principal label to retain identity and retry semantics;
changing the label is an identity rotation and must be coordinated across at
least one full idempotency retention window.

This is especially important for PostgreSQL run SSE: retained event payloads
are plaintext JSONB and can contain task output, reasoning, logs, agent prompts,
model content, and tool content. Durable `human_input_requested` events omit the
prompt and choices, and `human_input_received` never contains the answer, but
the rest of the journal is not a secret store. Every API token can read every
flow's protected journal, so tokens are administrator-equivalent rather than
per-flow read grants.

### Secrets handling

- **Never** bake API keys into the container image.
- Mount them as environment variables via `Secret` (Kubernetes), `Environment Variables` (Railway), or equivalent.
- Lua `env()` and `${env.NAME}` interpolation share the fail-closed `IRONCREW_ENV_ALLOWLIST`, so process secrets are unreadable unless explicitly opted in.
- MCP stdio children do **not** inherit the parent environment by default — only `PATH`, `HOME`, `USER`, `LANG`, `LC_*`. Secrets are therefore isolated from spawned MCP servers unless you explicitly list them in `env = {...}` or set `inherit_env = true`.
- MCP servers must implement the `2026-07-28` `server/discover` lifecycle and
  declare the `tools` capability; otherwise IronCrew closes the connection
  without sending `tools/list`.
  Streamable HTTP configuration must name the POST endpoint (normally `/mcp`),
  not a legacy `/sse` endpoint. IronCrew sends self-contained protocol/client
  metadata without creating an MCP session and never falls back to
  `initialize`.
- MCP tool calls support bounded, state-only MRTR. Non-empty `inputRequests`
  and Tasks-extension results fail closed because IronCrew advertises neither
  capability. The overall call timeout covers every retry and backoff.
- A deadline or caller cancellation permanently closes the local MCP
  connection. On Unix, stdio groups are synchronously killed; the owned supervisor
  reaps the direct child, and explicit shutdown waits for confirmation. Windows
  deployments must use Streamable HTTP. HTTP
  requests/SSE paths close locally and are not reused. Neither transport can
  roll back remote side effects already performed.

---

## Resource limits (RAM/CPU)

### Tune these to your pod limits

| Variable | Default | Purpose |
|---|---|---|
| `IRONCREW_MAX_PROMPT_CHARS` | `102400` characters | Caps prompt size per task. |
| `IRONCREW_MAX_BODY_SIZE` | `10485760` (10 MiB) | Request body cap (hard ceiling 64 MiB). |
| `IRONCREW_HTTP_MAX_REQUEST_HEADER_BYTES` | `65536` (64 KiB) | Outbound `http_request` header budget (hard ceiling 1 MiB). |
| `IRONCREW_HTTP_MAX_REQUEST_BODY_BYTES` | `8388608` (8 MiB) | Outbound `http_request` body cap (hard ceiling 64 MiB). |
| `IRONCREW_HTTP_MAX_RESPONSE_BYTES` | `8388608` (8 MiB) | HTTP tool/Lua HTTP body cap. |
| `IRONCREW_HTTP_MAX_OUTPUT_BYTES` | `16777216` (16 MiB) | Final serialized `http_request` result cap. |
| `IRONCREW_PROVIDER_MAX_REQUEST_BYTES` | `33554432` (32 MiB) | Serialized provider JSON request cap, enforced before network send (hard ceiling 256 MiB). |
| `IRONCREW_PROVIDER_MAX_RESPONSE_BYTES` | `16777216` (16 MiB) | Non-streaming model response cap. |
| `IRONCREW_PROVIDER_MAX_STREAM_BYTES` | `33554432` (32 MiB) | Raw model SSE stream cap. |
| `IRONCREW_PROVIDER_MAX_OUTPUT_BYTES` | `16777216` (16 MiB) | Accumulated model output/reasoning cap. |
| `IRONCREW_CHAT_HISTORY_MAX_BYTES` | `33554432` (32 MiB) | Aggregate in-memory provider history cap (hard ceiling 256 MiB). |
| `IRONCREW_MAX_REASONING_BYTES` | `1048576` (1 MiB) | Reasoning retained during one provider tool loop (hard ceiling 16 MiB). |
| `IRONCREW_MAX_IMAGE_BYTES` | `20971520` (20 MiB) | Per-image local/remote input cap. |
| `IRONCREW_WEB_SCRAPE_MAX_BYTES` | `2097152` (2 MiB) | Cap on `web_scrape` HTML download. |
| `IRONCREW_FILE_READ_MAX_BYTES` | `10485760` | Cap on single `file_read` result (hard ceiling 256 MiB). |
| `IRONCREW_FILE_WRITE_MAX_BYTES` | `10485760` | Cap on single `file_write` input (hard ceiling 256 MiB). |
| `IRONCREW_GLOB_MAX_FILES` | `500` | Per-call file limit for `file_read_glob` (hard ceiling 10000). |
| `IRONCREW_GLOB_MAX_BYTES` | `52428800` | Aggregate file-content limit for `file_read_glob` (hard ceiling 256 MiB). |
| `IRONCREW_GLOB_MAX_OUTPUT_BYTES` | `67108864` | Final serialized glob-result cap (hard ceiling 256 MiB). |
| `IRONCREW_FOREACH_MAX_ITEMS` | `100` | Maximum fan-out items in one task. |
| `IRONCREW_FOREACH_MAX_OUTPUT_BYTES` | `8388608` (8 MiB) | Aggregate serialized foreach result cap. |
| `IRONCREW_TASK_RESULT_MAX_OUTPUT_BYTES` | `8388608` (8 MiB) | Per-task output retained in the run result map (hard ceiling 32 MiB). |
| `IRONCREW_TASK_RESULT_MAX_REASONING_BYTES` | `4194304` (4 MiB) | Per-task reasoning retained in the run result map (hard ceiling 16 MiB). |
| `IRONCREW_RUN_RESULTS_MAX_BYTES` | `33554432` (32 MiB) | Aggregate serialized task results retained for one run (hard ceiling 48 MiB). |
| `IRONCREW_SHELL_MAX_OUTPUT_BYTES` | `1048576` | Shell stdout/stderr cap. |
| `IRONCREW_MCP_TOOL_RESULT_MAX_BYTES` | `262144` | Cap on each MCP tool result. |
| `IRONCREW_MCP_MAX_MRTR_ROUNDS` | `10` | Total wire attempts for one state-only MCP multi-round tool call. |
| `IRONCREW_MCP_MAX_REQUEST_STATE_BYTES` | `65536` | UTF-8 byte cap for opaque MCP `requestState`. |
| `IRONCREW_MCP_MAX_INBOUND_MESSAGE_BYTES` | `1048576` | Pre-decode byte cap for one MCP stdio line, HTTP JSON message, or SSE event. |
| `IRONCREW_DEFAULT_MAX_CONCURRENT` | `4` | Default task semaphore per execution phase; also bounds `foreach_parallel` fan-out unless the crew overrides `max_concurrent`. |
| `IRONCREW_MAX_CONCURRENT_TASKS` | `32` | Process policy ceiling for any crew's task semaphore. |
| `IRONCREW_MAX_AGENTS` | `64` | Maximum agents registered in one crew. |
| `IRONCREW_MAX_TASKS` | `256` | Maximum tasks registered in one crew. |
| `IRONCREW_CREW_GOAL_MAX_BYTES` | `65536` | Maximum crew-goal bytes (hard ceiling 1 MiB). |
| `IRONCREW_MAX_APPROVAL_PATTERNS` | `128` | Maximum crew approval patterns (hard ceiling 1024). |
| `IRONCREW_MAX_MEMORY_ITEMS` | `10000` | Policy ceiling for a crew's memory item setting (hard ceiling 100000). |
| `IRONCREW_MAX_MEMORY_TOKENS` | `1000000` | Policy ceiling for a crew's estimated memory tokens (hard ceiling 10000000). |
| `IRONCREW_MAX_SERVER_TOOLS` | `16` | Maximum provider-hosted tools per crew (hard ceiling 64). |
| `IRONCREW_MAX_VECTOR_STORE_IDS` | `32` | Maximum Responses vector-store IDs per crew (hard ceiling 256). |
| `IRONCREW_MAX_MODEL_ROUTES` | `64` | Maximum purpose-to-model routes per crew (hard ceiling 256). |
| `IRONCREW_LUA_MAX_MEMORY_BYTES` | `33554432` (32 MiB) | Allocator cap for each live Lua VM. |
| `IRONCREW_LUA_MAX_INSTRUCTIONS` | `50000000` | Per-top-level-execution instruction budget. |
| `IRONCREW_LUA_MAX_EXECUTION_SECONDS` | `1800` | Per-top-level-execution wall-clock budget. |
| `IRONCREW_MAX_EVENTS` | `1000` | Per-run replay count cap: in-memory for JSON/SQLite and logical journal retention for PostgreSQL. |
| `IRONCREW_EVENT_REPLAY_MAX_BYTES` | `4194304` (4 MiB) | Per-run replay byte budget: in-memory for JSON/SQLite and logical journal retention for PostgreSQL. |
| `IRONCREW_EVENT_MAX_BYTES` | `262144` (256 KiB) | Maximum serialized size of one live or durable event. |
| `IRONCREW_EVENT_CHANNEL_CAPACITY` | `32` | Live broadcast-ring entry cap per EventBus; automatically reduced to fit the replay byte budget. |
| `IRONCREW_MAX_SSE_CONNECTIONS` | `16` | Process-wide admission cap for long-lived run and conversation SSE connections. |
| `IRONCREW_MESSAGEBUS_QUEUE_DEPTH` | `1000` | Max pending messages per agent. |
| `IRONCREW_MESSAGEBUS_MESSAGE_MAX_BYTES` | `65536` | Cap on one inter-agent message. |
| `IRONCREW_MESSAGEBUS_QUEUE_MAX_BYTES` | `4194304` (4 MiB) | Byte cap on each agent queue. |
| `IRONCREW_MAX_RUN_LIFETIME` | `1800` (30 min) | Hard per-run timeout. Lower for short flows. |
| `IRONCREW_MAX_CONVERSATION_TURN_SECS` | `300` | Whole provider/tool deadline for one conversation message. |
| `IRONCREW_READINESS_CACHE_MS` | `1000` | Caches public storage-aware readiness results to protect the DB pool. Overlapping uncached probes share the in-flight check for up to one second, then fail closed instead of returning a false contention-only `503`. |
| `IRONCREW_CONVERSATION_MAX_HISTORY` | `50` | Trim conversation history at this many non-system messages (hard ceiling 4096; zero is rejected). |
| `IRONCREW_DIALOG_MAX_HISTORY` | `100` | Trim dialog transcript at this many turns (hard ceiling 4095). |
| `IRONCREW_DIALOG_MAX_TURNS` | `1000` | Maximum accepted total turns in one dialog (hard ceiling 10000). |
| `IRONCREW_DIALOG_MAX_PARTICIPANTS` | `16` | Maximum accepted participants in one dialog (hard ceiling 64). |
| `IRONCREW_MAX_ACTIVE_CONVERSATIONS` | `8` | Max simultaneous live HTTP chat sessions in this process. Exceeding returns 503. |
| `IRONCREW_MAX_CONVERSATION_LIFECYCLES` | `256` | Bounds distinct conversation IDs with an in-flight lifecycle operation, preventing unbounded coordination-map growth (hard ceiling 4096). |
| `IRONCREW_MAX_ACTIVE_RUNS` | `4` | Max simultaneous in-flight flow runs (`POST /flows/{flow}/run`). Exceeding returns 503. |
| `IRONCREW_REQUIRE_IDEMPOTENCY_KEY` | `false` | Set `true` in production so run and JSON/SQLite message retries cannot silently duplicate work. PostgreSQL conversation messages require a key regardless. |
| `IRONCREW_IDEMPOTENCY_TTL_SECONDS` | `86400` | Replay/tombstone retention; must exceed max run lifetime by at least one hour. |
| `IRONCREW_IDEMPOTENCY_MAX_RESPONSE_BYTES` | `8388608` | Per-key transient serialization and stored-response cap. Lower to 4 MiB for the 1 GiB baseline. |
| `IRONCREW_IDEMPOTENCY_MAX_TOTAL_RESPONSE_BYTES` | `268435456` | Aggregate stored response budget. Lower to 64 MiB for the 1 GiB baseline. |
| `IRONCREW_IDEMPOTENCY_MAX_RECORDS_PER_PRINCIPAL` | global record cap | Durable record budget for one authenticated principal; never exceeds the global cap. |
| `IRONCREW_IDEMPOTENCY_MAX_IN_FLIGHT_PER_PRINCIPAL` | min(global record cap, 64) | Maximum concurrent claimed/in-progress mutations for one principal. |
| `IRONCREW_IDEMPOTENCY_MAX_TOTAL_RESPONSE_BYTES_PER_PRINCIPAL` | global response-byte cap | Durable completed-response budget for one principal; never exceeds the global byte cap. |
| `IRONCREW_COLLABORATION_MAX_TRANSCRIPT_BYTES` | `8388608` (8 MiB) | Aggregate retained transcript for one collaborative task (hard ceiling 32 MiB). |
| `IRONCREW_COLLABORATION_MAX_TURN_BYTES` | `1048576` (1 MiB) | Maximum one collaborative provider response may add (hard ceiling 8 MiB). |
| `IRONCREW_COLLABORATION_MAX_PARTICIPANT_TURNS` | `64` | Maximum participants multiplied by turns (hard ceiling 512). |
| `IRONCREW_CHAT_SESSION_IDLE_SECS` | `1800` (30 min) | Idle window after which a chat handle is evicted from memory. |
| `IRONCREW_CONVERSATIONS_DEFAULT_LIMIT` | `20` | Default page size for `GET /flows/{flow}/conversations`. |
| `IRONCREW_CONVERSATIONS_MAX_LIMIT` | `100` | Hard cap on `?limit=` for the conversation list endpoint. |
| `IRONCREW_RUNS_DEFAULT_LIMIT` | `20` | Default page size for `GET /flows/{flow}/runs`. |
| `IRONCREW_RUNS_MAX_LIMIT` | `100` | Hard cap on `?limit=` for the run list endpoint. |
| `IRONCREW_MAX_FLOW_DEPTH` | `5` | Maximum recursion depth for `run_flow` sub-flow invocation. Raise only if you have intentional deep nesting. |
| `IRONCREW_TOOL_TIMEOUT` | `60` | Seconds to wait before a single tool call is cancelled (hard ceiling 3600; invalid or zero uses 60). |
| `IRONCREW_SSE_OUTPUT_MAX_CHARS` | _off_ | Optional response-only task-output truncation for process-local JSON/SQLite run SSE. PostgreSQL durable events use `IRONCREW_EVENT_MAX_BYTES` instead. |

The conversation-related defaults above are only generic fallbacks. For Cloud
deployments, especially when using HTTP chat and SSE, size them intentionally
using the guidance in [HTTP Scaling](http-scaling.md). The complete list,
including API image/message, memory, ask-human, schema, MCP, and database
ceilings, is in [CLI environment variables](cli.md#environment-variables).

PostgreSQL does not move all journal limits out of pod RAM. Each active run has
the normal bounded EventBus replay plus a durable producer queue (at most 64
events and, by default, 1 MiB, enlarged only enough for the configured
single-event limit without exceeding the per-run budget). Each journal reader
also fetches a bounded page before writing SSE. Multiply those allocations, Lua
VMs, provider buffers, database pool connections, and SSE connections by
replica count. Leave container headroom for allocator overhead and shutdown
spikes; Railway Pro's account ceiling and an OpenShift namespace quota are not
per-process memory targets.

### Recommended baselines

**Small pod (256 MB / 0.25 CPU):**
```bash
IRONCREW_MAX_ACTIVE_RUNS=1
IRONCREW_MAX_ACTIVE_CONVERSATIONS=2
IRONCREW_CHAT_SESSION_IDLE_SECS=300
IRONCREW_LUA_MAX_MEMORY_BYTES=16777216
IRONCREW_CHAT_HISTORY_MAX_BYTES=4194304
IRONCREW_API_MAX_IMAGE_BYTES_PER_CONVERSATION=4194304
IRONCREW_MAX_PROMPT_CHARS=30000
IRONCREW_MAX_BODY_SIZE=2097152
IRONCREW_HTTP_MAX_REQUEST_HEADER_BYTES=32768
IRONCREW_HTTP_MAX_REQUEST_BODY_BYTES=2097152
IRONCREW_HTTP_MAX_RESPONSE_BYTES=2097152
IRONCREW_PROVIDER_MAX_OUTPUT_BYTES=4194304
IRONCREW_PROVIDER_MAX_RESPONSE_BYTES=4194304
IRONCREW_PROVIDER_MAX_STREAM_BYTES=4194304
IRONCREW_MAX_IMAGE_BYTES=2097152
IRONCREW_DEFAULT_MAX_CONCURRENT=2
IRONCREW_MAX_CONCURRENT_TASKS=2
IRONCREW_CREW_GOAL_MAX_BYTES=32768
IRONCREW_MAX_APPROVAL_PATTERNS=32
IRONCREW_MAX_MEMORY_ITEMS=1000
IRONCREW_MAX_MEMORY_TOKENS=100000
IRONCREW_MAX_SERVER_TOOLS=8
IRONCREW_MAX_VECTOR_STORE_IDS=8
IRONCREW_MAX_MODEL_ROUTES=16
IRONCREW_FOREACH_MAX_ITEMS=25
IRONCREW_FOREACH_MAX_OUTPUT_BYTES=2097152
IRONCREW_TASK_RESULT_MAX_OUTPUT_BYTES=2097152
IRONCREW_TASK_RESULT_MAX_REASONING_BYTES=1048576
IRONCREW_RUN_RESULTS_MAX_BYTES=8388608
IRONCREW_MAX_EVENTS=100
IRONCREW_EVENT_REPLAY_MAX_BYTES=524288
IRONCREW_EVENT_MAX_BYTES=131072
IRONCREW_EVENT_CHANNEL_CAPACITY=4
IRONCREW_MAX_SSE_CONNECTIONS=8
```

**Medium pod (1 GiB / 1 CPU):** the checked-in OpenShift manifest is a
conservative, unbenchmarked baseline: 2 active runs, 4 resident conversations,
256 in-flight conversation lifecycle keys, concurrency 2 with a hard ceiling
of 4, 24 MiB per Lua VM, principal admission and ledger quotas, bounded
history/images/network/provider output, and a PostgreSQL pool of 2. Keep these
settings until a workload-specific Linux/container soak test demonstrates
headroom.

At two identical replicas this declares 4 active runs, 8 resident
conversations, 32 SSE connections, 4 maximum PostgreSQL pool connections, 2
GiB of container memory allocation, and a 288 MiB admitted run/conversation Lua
top-level allocator budget. Nested `run_flow`/`crew:subworkflow` VMs are
additional, so this is neither an RSS bound nor proof that the workload fits
the container allocation. It also does not create a cluster-wide provider
limit. See
[multi-replica capacity arithmetic](http-scaling.md#multi-replica-capacity-arithmetic)
for the formulas, shared-quota exceptions, and provider caveat.

**Large pod (4 GB / 4 CPU, after load testing):**
```bash
IRONCREW_MAX_ACTIVE_RUNS=4
IRONCREW_MAX_ACTIVE_CONVERSATIONS=16
IRONCREW_DEFAULT_MAX_CONCURRENT=4
IRONCREW_MAX_EVENTS=250
IRONCREW_EVENT_REPLAY_MAX_BYTES=2097152
IRONCREW_EVENT_CHANNEL_CAPACITY=16
IRONCREW_MAX_SSE_CONNECTIONS=32
```

Do not infer safe application limits from the Railway Pro account ceiling. Pro
permits substantially larger aggregate resources, but IronCrew should still
have explicit per-service CPU/RAM limits and conservative application caps.

### Rate limiting

- `IRONCREW_RATE_LIMIT_MS` — per-provider minimum interval between LLM calls (milliseconds). Use to stay within provider-side quotas.
- `IRONCREW_DEFAULT_MAX_CONCURRENT` limits task parallelism inside each crew run; it is not a process-wide provider limit.
- `IRONCREW_MAX_ACTIVE_RUNS` and `IRONCREW_MAX_ACTIVE_CONVERSATIONS` are process-wide admission limits for the HTTP server.
- `IRONCREW_ADMISSION_WORK_RATE_PER_MINUTE` / `IRONCREW_ADMISSION_WORK_BURST`
  apply a per-principal token bucket before run and conversation-message work
  begins (defaults `60` / `10`).
- `IRONCREW_ADMISSION_CONTROL_RATE_PER_MINUTE` /
  `IRONCREW_ADMISSION_CONTROL_BURST` separately bound lower-cost control-plane
  mutations such as cancellation and answers (defaults `120` / `20`).
- `IRONCREW_ADMISSION_OBSERVATION_RATE_PER_MINUTE` /
  `IRONCREW_ADMISSION_OBSERVATION_BURST` bound question-list observation per
  principal and process (defaults `600` / `20`; ranges `1–60000` / `1–1000`).
  They do not throttle internal PostgreSQL journal or HITL owner polling.

Admission is process-local and intentionally low-cardinality: the supported
limits multiply with HTTP replica count. Durable idempotency quotas are
enforced by the store and remain the restart-safe protection. A `429` includes
`Retry-After`; clients should back off without changing the idempotency key for
the same logical operation.

If an application requires one cluster-wide request, concurrency, or provider
budget, enforce it in a trusted shared gateway that authenticates before
policy, bounds queues/waits, fails closed, and preserves idempotency keys.
PostgreSQL is used for durable ledger/journal coordination; do not hold a
database lock across provider or tool execution to simulate a global
semaphore.

---

## Persistence

IronCrew can store run records in three backends. **Choose based on your platform's volume story**.

| Backend | Best for | Env |
|---|---|---|
| JSON files | single-pod deployments with mounted PVC | `IRONCREW_STORE=json` (default) |
| SQLite | single-pod or small team, self-contained | `IRONCREW_STORE=sqlite`, `IRONCREW_STORE_PATH=/data/ironcrew.db` |
| PostgreSQL | durable production storage and safe restart recovery | `IRONCREW_STORE=postgres`, `DATABASE_URL=postgres://...` |

**Kubernetes/OpenShift:** use PostgreSQL for production durability. With a
shared HITL keyring, idempotency-keyed runs support cross-replica cancellation
and question listing/answer delivery. PostgreSQL's bounded run-event journal
also supports cross-replica run SSE replay. The committed IC-008 implementation
provides cold rehydration of a keyed PostgreSQL conversation message on
either replica from the last committed incarnation/revision, but does not move
an in-flight turn. Its local two-process PostgreSQL gate and affinity-free
OpenShift IC-008 canary pass; Railway remains unrun, and the tested dirty
artifact was unpublished and removed. Shared-store conversation SSE returns
`409` unsupported. JSON/SQLite run and conversation SSE remain owner-local.
Keep `replicas: 1` until a published release contains the behavior, or whenever
clients require the remaining owner-local surfaces through arbitrary Service
routing.
JSON/SQLite require a writable persistent volume if their records must survive
replacement.

**Railway:** the built-in PostgreSQL add-on is the simplest path. Add it and Railway sets `DATABASE_URL` automatically.

### Live-control boundary

The safest general-purpose topology remains one `serve` replica because a
second process cannot take over a dead owner's in-flight Lua VM. PostgreSQL
does support a bounded multi-replica slice: any replica can accept new keyed
runs and durable reads, stream retained run journal events, replay keyed
acceptance, request keyed cancellation,
and—when the shared HITL keyring is configured—list or answer that keyed run's
pending questions. The committed IC-008 implementation also reconstructs a
required-key `/messages` request on either replica from a committed
conversation boundary.
The local process gate and OpenShift canary pass; Railway routing remains
unrun, the artifact is unpublished, in-flight takeover is unsupported, and
conversation SSE remains unsupported. Routing affinity is not a correctness
mechanism for the remaining owner-local surfaces. Use one-shot `ironcrew run`
workers only when their workload and store ownership are intentionally
separated from the HTTP service.

**Store lifecycle in `serve` mode.** The store is a **server-wide singleton**: it is bootstrapped once per process startup and reused across all request handlers. With the PostgreSQL backend this means migrations (`CREATE TABLE IF NOT EXISTS`, `ALTER TABLE ADD COLUMN IF NOT EXISTS`, index creation) run once during each process boot, and the SQLx connection pool is shared across every concurrent request. Size `IRONCREW_DB_POOL_SIZE` for the number of concurrent in-flight requests, not the number of flows mounted in `--flows-dir`.

The current PostgreSQL runtime role is also the bootstrap/migration role. Each
process starts an atomic bootstrap transaction, takes an exclusive
transaction-scoped advisory lock, and may create or alter IronCrew tables and
indexes and create or replace functions and triggers before verifying the
schema. The configured role therefore needs schema-owner-like DDL permissions
over the IronCrew schema objects today; a DML-only runtime role is not
supported. A separate migration job plus a least-privileged runtime role is a
future hardening step; do not revoke DDL permissions from the configured role
yet.

**Terminal-write outage memory bound.** A healthy terminal write persists the
full task-result payload. If storage rejects that write, IronCrew will retain
at most 1 MiB of task-result allocations for one additional full-payload
attempt. Larger payloads are released after the first failed attempt; smaller
payloads are released after the second. Terminal status, timing, and aggregate
token counts continue retrying until durable, admission stays occupied, and
readiness remains down. Consequently, a run finalized during a sustained
storage outage can eventually have an empty `task_results` array even though
its terminal metadata is correct. This deliberate degradation prevents every
admitted run from pinning up to the full run-result ceiling in Railway or
OpenShift pod RAM while PostgreSQL is unavailable; the release is logged with
the run ID and dropped-result count.

### Postgres-specific

| Variable | Default | Description |
|---|---|---|
| `DATABASE_URL` | — | PostgreSQL 15+ DSN. Required. |
| `IRONCREW_PG_TABLE_PREFIX` | empty | Prefix for shared databases (e.g. `tenant1_`), max 37 lowercase ASCII alphanumeric/underscore bytes. |
| `IRONCREW_DB_POOL_SIZE` | `10` | Connection pool size (range 1–128). Raise only for measured concurrent load. |
| `IRONCREW_DB_CONNECT_RETRIES` | `10` | Connection retries after the initial attempt (range 0–100). |
| `IRONCREW_DB_CONNECT_BACKOFF_MS` | `1000` | Base delay for exponential connection-retry backoff, in milliseconds (range 1–30000). |
| `IRONCREW_DB_CONNECT_TIMEOUT_SECS` | `30` | Connect/acquire timeout (range 1–120 seconds). |
| `IRONCREW_INSTANCE_ID` | Railway runtime replica ID, otherwise generated per process | Optional 1–255 byte printable ASCII runtime identity written to run ownership records. An explicit value wins; otherwise IronCrew uses a valid non-empty runtime `RAILWAY_REPLICA_ID`, then falls back to a generated process identity. Use the pod UID explicitly on OpenShift. Do not configure Railway's runtime-only value through service-reference interpolation. |
| `IRONCREW_RUN_LEASE_TTL_SECONDS` | `60` | Ownership lease expiry before unfinished-run reconciliation, and the grace before explicit indeterminate-turn recovery. Production range: 6–86400; owner heartbeat and replica maintenance cadence is TTL/3. |
| `IRONCREW_HITL_ENCRYPTION_KEYS` | unset | Secret JSON object of at most 8 key ids and canonical base64 32-byte keys; maximum 16 KiB. Enables the encrypted mailbox only when the active id is also set. |
| `IRONCREW_HITL_ACTIVE_KEY_ID` | unset | Key id used for newly registered question ciphertext. Configure identically in steady state; the ordered rotation below deliberately permits a mixed-active overlap only after every process has both keys. |
| `IRONCREW_ASK_HUMAN_MAX_PENDING_BYTES` | `1048576` (1 MiB) | Aggregate serialized pending-question metadata per run; range 1 byte–16 MiB. PostgreSQL ciphertext admission also accounts for 28 AEAD bytes per allowed pending row. |
| `IRONCREW_HITL_POLL_INTERVAL_MS` | `500` | PostgreSQL poll interval per pending durable question. Effective range 50–5000 ms; tune against latency and aggregate database reads. |
| `IRONCREW_HITL_READ_TIMEOUT_MS` | `2000` | Timeout for one owner-side PostgreSQL answer read. Effective range 100–30000 ms. |
| `IRONCREW_HITL_PG_MAX_CONCURRENT_READS` | `8` | Process-wide concurrency cap shared by PostgreSQL question-list decryption and answer-side question authentication; range 1–64. |
| `IRONCREW_EVENT_JOURNAL_RETENTION_SECS` | `3600` | Logical PostgreSQL event retention; range 60–2592000 seconds. |
| `IRONCREW_EVENT_JOURNAL_MAX_TOTAL_EVENTS` | `100000` | Global logical event-count cap; range 1–10000000 and at least `IRONCREW_MAX_EVENTS`. |
| `IRONCREW_EVENT_JOURNAL_MAX_TOTAL_BYTES` | `268435456` (256 MiB) | Global logical event-byte cap; range 1 KiB–8 GiB and at least the per-run replay-byte budget. |
| `IRONCREW_EVENT_JOURNAL_PAGE_MAX_BYTES` | `524288` (512 KiB), or the event maximum when larger | Maximum bytes read in one journal page; range 1 KiB–64 MiB and at least `IRONCREW_EVENT_MAX_BYTES`. A page contains at most 64 events. |
| `IRONCREW_EVENT_JOURNAL_POLL_INTERVAL_MS` | `500` | Active-stream database poll interval; range 100–5000 ms. |
| `IRONCREW_EVENT_JOURNAL_READ_TIMEOUT_MS` | `2000` | Timeout for one journal page read; range 100–30000 ms. |
| `IRONCREW_EVENT_JOURNAL_WRITE_TIMEOUT_MS` | `1500` | Outer timeout `W` for one journal-append attempt, including pool acquisition and the complete transaction; range 100–5000 ms. PostgreSQL statements use `4W/5`. |
| `IRONCREW_EVENT_JOURNAL_PRUNE_BATCH` | `1000` | Maximum rows pruned per bounded pass; range 1–10000 and no greater than the global event cap. |
| `IRONCREW_ADMISSION_OBSERVATION_RATE_PER_MINUTE` | `600` | Per-principal/process rate for question-list observation; range 1–60000. |
| `IRONCREW_ADMISSION_OBSERVATION_BURST` | `20` | Per-principal/process observation burst; range 1–1000. |

IronCrew supports PostgreSQL 15+ only. This matches the session-storage
features used by the runtime and the intended deployment target of
extension-capable Postgres installs such as `pgvector`.

### Deployment evidence and replica parity

Authenticated `GET /capabilities` always reports both the `instance_id`
recorded in durable run ownership and a random UUID `process_start_id` created
for the current operating-system process. A platform may reuse its replica id
after restart; `process_start_id` distinguishes the replacement process without
becoming a durable owner or routable address.

For release-candidate and platform acceptance, configure the optional
deployment-evidence tuple:

| Variable | Capability field | Contract |
|---|---|---|
| `IRONCREW_DEPLOYMENT_REVISION` | `deployment.revision` | Immutable source/build-input identifier, 1–128 ASCII letters, digits, `.`, `-`, `_`, `:`, or `+` |
| `IRONCREW_ARTIFACT_FINGERPRINT` | `deployment.artifact_fingerprint` | Fingerprint of the exact executable/artifact in this process |
| `IRONCREW_FLOW_FINGERPRINT` | `deployment.flow_fingerprint` | Fingerprint of the complete canonical flow-tree manifest |
| `IRONCREW_CONFIG_FINGERPRINT` | `deployment.config_fingerprint` | Fingerprint of canonical effective non-secret application settings |
| `IRONCREW_HITL_KEYRING_FINGERPRINT` | `deployment.hitl_keyring_fingerprint` | Fingerprint of the canonical non-secret readable-keyset/active-key revision |

All five variables are optional only as one unit. With all absent,
`deployment` is `null`; any partial tuple fails startup before the HTTP listener
binds. Each fingerprint must be exactly `sha256:` followed by 64 lowercase
hexadecimal characters. The tuple is protected and non-cacheable because it is
returned only by `/capabilities`.

IronCrew validates and repeats these operator-supplied values; it does not
calculate or verify them. Equal reported strings are therefore not platform
parity proof by themselves. A valid acceptance receipt must:

1. freeze and retain the build-input/revision manifest;
2. document the exact deterministic serialization used for the flow, effective
   config, and keyring-revision manifests;
3. inventory every active Railway replica or OpenShift pod rather than relying
   on a finite load-balancer sample;
4. independently hash the executable and the three canonical manifests inside
   every process without printing their source secrets; and
5. compare the independently observed values with that process's authenticated
   capability tuple, `X-IronCrew-Instance-Id`, and `process_start_id`.

The config manifest must use resolved effective values—not merely whichever
environment variables happened to be present—and include the non-secret
settings whose equality is required: storage type and table/schema identity,
authentication/principal policy shape, idempotency policy, database pool and
lease policy, admission/lifecycle/journal policy, and relevant runtime limits.
Explicitly configure every required field or derive its resolved default before
hashing. Do not include raw `DATABASE_URL`, bearer/provider credentials, raw
HITL key material, or unsalted hashes of guessable secrets. Verify the canary
bearer against every process separately.

For the keyring revision, canonicalize the key ids, active id, and fingerprints
derived from the random 32-byte keys; never include or log the base64 key
material. This detects the otherwise invisible error where matching key ids map
to different keys. During the ordered rotation below, config/keyring hashes can
intentionally differ between compatible revisions. Record the allowed value for
every process and phase rather than declaring any difference acceptable.

Unique `IRONCREW_INSTANCE_ID`/`RAILWAY_REPLICA_ID` values,
`process_start_id`, pod/deployment ids, injected bind addresses and ports,
timestamps, and pod-specific paths are attribution data and are excluded from
steady-state fingerprint equality. Platform CPU/memory requests, limits, and
physical replica/surge counts are also outside the application fingerprint;
record and compare them separately because they remain required capacity
evidence.

### Lease maintenance, readiness, and owner loss

Production lease TTLs must be 6–86400 seconds. IronCrew schedules owner
heartbeats and replica maintenance every `TTL / 3` (2 seconds at the minimum;
20 seconds at the 60-second default). Keyed-run and conversation fences start
their local deadline when a storage claim or heartbeat is invoked, not when a
slow response arrives, so database latency cannot extend local side effects
past the durable lease window.

PostgreSQL maintenance has an inner transaction-local `lock_timeout` and
`statement_timeout`, followed by a larger outer Tokio watchdog that also covers
pool acquisition and setup. With cadence `C`, the outer per-operation bound is
`W = min(5 seconds, max(100 ms, C / 3))`; the inner database bound is
`max(50 ms, 4W / 5)`. The heartbeat and reconciliation operations run
sequentially. At the default TTL they are bounded to 5 seconds each outside the
database and 4 seconds per statement inside it. The inner limit resolves one
blocked statement; the outer limit also covers cumulative statement latency.
If the outer watchdog wins before core reconciliation commits, the owned SQLx
transaction is dropped and rolled back. PostgreSQL 15 regressions verify that
atomic rollback and pool recovery. Best-effort run-event pruning happens in a
later non-authoritative transaction: a timeout there cannot undo committed
run/idempotency/HITL recovery, although readiness can remain down until the
next complete maintenance cycle.

Reconciliation retains at most 64 run IDs per transaction and scopes all
dependent journal, idempotency, and mailbox writes to that batch. This bounds
per-cycle query results and application RAM on Railway and OpenShift pods. At
the default cadence, a saturated backlog drains at roughly 192 runs per minute;
alert on persistent batch saturation rather than lowering the lease TTL.

If startup reconciliation or PostgreSQL idempotency pruning times out, the HTTP
process can still bind, but `/health/ready` reports `503` with
`component: "storage_maintenance"`. A later bounded heartbeat or reconciliation
failure drops readiness; one complete successful cycle is required to restore
it. Healthy in-flight maintenance does not create a transient `503`. Keep
`/health/live` as liveness so an advisory-lock or database incident withdraws
an OpenShift/Kubernetes pod from Service routing without creating a restart
loop.

For a continuously running healthy peer, the nominal dead-owner observation
window is `TTL + one cadence + two outer operation bounds` when the run fits in
the next 64-row batch: at most about 90 seconds with the 60-second default, or
9.332 seconds at the 6-second minimum. A larger expired backlog adds cadence
windows. This is not execution failover and is not a hard bound during
persistent database unavailability, repeated contention, or scheduler
starvation. Size the TTL for measured database latency and scheduling jitter;
do not lower it to accelerate platform replacement.

OpenShift/Kubernetes readiness probes continuously observe this maintenance
state. Railway's configured healthcheck is a deployment gate rather than a
continuous runtime-routing control, so monitor `/health/ready` externally and
alert on `storage_maintenance` after deployment. Neither platform should use
the lease TTL as a substitute for its SIGTERM/drain/SIGKILL budget.

### HITL key rotation on Railway and OpenShift

Treat `IRONCREW_HITL_ENCRYPTION_KEYS` as encryption-key material, not ordinary
configuration. Put it in a Railway secret variable or an
OpenShift/Kubernetes `Secret` supplied by an external secret manager. The JSON
values must be canonical base64 encodings of exactly 32 random bytes.

IronCrew reads the keyring once during process startup; editing a Railway
variable or OpenShift Secret does not reload a running process. Rotate through
three ordered rollout revisions without making in-flight questions unreadable:

1. Deploy the expanded `{old,new}` keyring to every process with `old` still
   active. Do not proceed until every known replica has restarted with both
   readable keys.
2. Deploy the same expanded keyring with `new` active. Mixed old-active and
   new-active processes are safe only in this phase because every reader has
   both keys. The active id selects new question ciphertext; an accepted answer
   is encrypted with its authenticated question's fingerprint, so an old-active
   owner can still consume an answer queued by a new-active peer.
3. After every old-active writer has exited, drain or terminalize old-key
   questions and verify that both `question_key_fingerprint` and
   `answer_key_fingerprint` have zero references to the old fingerprint. Only
   then deploy a new-only keyring.

Use a bound parameter for the retiring fingerprint when applying this database
gate to the configured table prefix:

```sql
SELECT
    COUNT(*) FILTER (WHERE question_key_fingerprint = $1) AS old_question_refs,
    COUNT(*) FILTER (WHERE answer_key_fingerprint = $1) AS old_answer_refs,
    COUNT(*) FILTER (
        WHERE question_key_fingerprint = $1 OR answer_key_fingerprint = $1
    ) AS old_rows
FROM {prefix}human_inputs;
```

Every count must be zero. Store startup also scans both fingerprint columns and
fails before the HTTP listener binds when retained ciphertext needs an absent
key. The scan is bounded by `IRONCREW_DB_CONNECT_TIMEOUT_SECS` plus one second
and is a startup snapshot, not a recurring fleet audit: a stale old-active
writer that appears afterward is rejected when a replica authenticates the
mailbox row, but operators must still inventory and stop every old revision.
`/health/ready` and `/capabilities` do not replace that fleet check.

Railway rolling deployments and OpenShift rolling updates can temporarily run
both revisions. A one-step key replacement is therefore unsafe: an old pod
cannot read new-key ciphertext, while a new pod cannot read old-key ciphertext.
The keyring supports at most eight keys, which leaves room for staged rotation
without unbounded secret or process memory. Keep the compatible expanded secret
available during code rollback; restoring an old-only process after new-key
questions exist is intentionally rejected.

At the default 500 ms poll interval, each pending question performs about two
PostgreSQL reads per second. Multiply that by pending questions and suspended
runs across all pods; lower `IRONCREW_ASK_HUMAN_MAX_PENDING` or increase the
poll interval when Railway/OpenShift database IOPS or pool usage matters more
than sub-second answer pickup. Prompt, aggregate-choice, and answer defaults
are each 64 KiB, and pending questions default to 16 per run. Prompt and
aggregate choices each have a 1 MiB hard ceiling, answers have a 1 MiB hard
ceiling, and pending count has a 256 hard ceiling. The aggregate serialized
pending-question metadata cap is 1 MiB by default and 16 MiB at its hard
maximum, so the independent field/count maxima cannot combine into an
unbounded pending map. Keep the aggregate cap conservative on small pods and
leave headroom for JSON, encryption, map, and allocator overhead.

### PostgreSQL event-journal operations

Successful PostgreSQL run streams can be served by any replica, emit
`id: <run_id>:<sequence>`, honor `Last-Event-ID`, and set
`Cache-Control: no-store, no-transform` plus `X-Accel-Buffering: no`.
Malformed or cross-run cursors return `400`; ahead/expired cursors and replay
requests against JSON/SQLite return `409`. Gaps caused by writer backpressure,
retention, global capacity, or owner loss are explicit. Active-run completeness
is best-effort, and a terminal run record can synthesize an unnumbered
`run_complete` with `journal_complete: false` when the numbered terminal event
is absent.

Let `W` be `IRONCREW_EVENT_JOURNAL_WRITE_TIMEOUT_MS`. One journal batch gets
three outer attempts with 50 ms and 100 ms backoffs (`3W + 150 ms` total). Each
append transaction sets PostgreSQL `lock_timeout` and `statement_timeout` to
`4W/5`, so one database lock/query wait ends before the outer deadline; that
outer deadline also covers pool acquisition and cumulative statements. The
pre-terminal flush and numbered terminal append each use a derived
acknowledgement deadline of `3W + 650 ms`. This is a bounded best-effort wait,
not a guarantee to drain every queued batch. The authoritative terminal run row
is committed before the numbered terminal append, so terminal run status is not
a journal-flush barrier. Idempotency finalization runs before the bounded
terminal append begins, so clients that need a resumable terminal cursor must
use their own bounded reconnect policy. The run row
still progresses when the journal fails, enabling the explicit incomplete SSE
fallback above.

The per-run/global byte caps are logical accounting, with at least 1 KiB
charged per event. They do not include PostgreSQL tuple/page or index overhead,
the runs/state/usage tables, WAL, replication/backups, or dead tuples awaiting
autovacuum. Expired rows become logically invisible immediately, while bounded
physical pruning runs on append and best-effort reconciliation. Monitor actual
database size and autovacuum independently; disk usage can exceed the logical
cap substantially.

---

## Observability

### Logs

- Tracing output → stderr. Logs do **not** mix with `run`-mode stdout.
- `IRONCREW_LOG` controls log level. Format: `env_logger` / `tracing` directive.

```bash
IRONCREW_LOG=info              # production default
IRONCREW_LOG=ironcrew=debug    # debug ironcrew-only
IRONCREW_LOG=debug,hyper=info  # broad debug, suppress hyper
```

### Health endpoints

All health endpoints are public and require no API token:

| Endpoint | Meaning | Use |
|---|---|---|
| `GET /health/live` | Process is alive and the HTTP runtime can answer. It does not probe providers or storage. | Kubernetes/OpenShift liveness. |
| `GET /health/ready` | Process is `accepting`, the configured persistence store responds, and run-lease maintenance is healthy. Lifecycle withdrawal returns `503` with `component: "lifecycle"` and the current `lifecycle_state`; other failures retain their component label. | Startup/readiness probes and Railway deployment healthcheck. |
| `GET /health` | Backwards-compatible lightweight liveness response. | Existing monitors only; do not use as a storage-aware readiness gate. |

Provider APIs are intentionally excluded from readiness: a provider outage
should fail requests cleanly, not continuously restart or withdraw the whole
service.

Authenticated protected responses include `X-IronCrew-Instance-Id`. Preserve
that header in canary tooling so a load-balanced request can be attributed to
the receiving process, but never turn the opaque value into a public route.
The configured CORS policy exposes it and `Retry-After` to authorized browser
clients.

### Metrics

`GET /metrics` exposes Prometheus text and is protected by the same bearer
authentication as the API. It reports process-wide active/limit gauges,
principal admission outcomes, tracked bucket count, idempotency quota
rejections, readiness-critical storage state, and a store-backed durable-ledger
snapshot. `ironcrew_store_maintenance_healthy` is an unlabeled per-process
gauge (`1` healthy, `0` unhealthy) for the latest completed
heartbeat-plus-reconciliation cycle. The unlabeled
`ironcrew_process_terminal_persistence_degraded_finalizers` gauge counts the
run or conversation finalizers currently retrying durable persistence in that
process. It is not a cumulative error counter: it returns to zero after
recovery or fencing, and both gauges reset when the process restarts.
`ironcrew_process_lifecycle_state` is a one-hot per-process gauge with the four
fixed `state` label values `accepting`, `fencing`, `draining`, and `stopping`.
Alert when a target remains outside `accepting` unexpectedly; a planned
explicit drain should be correlated with deployment events rather than treated
as storage failure.
`ironcrew_process_lifecycle_rejections_total{class="work|control"}` counts
direct lifecycle-boundary mutation rejections with two fixed class labels.

Execution and storage instrumentation uses only closed label vocabularies:

| Series | Type | Exact fixed labels |
|---|---|---|
| `ironcrew_runs_total`; `ironcrew_run_duration_seconds` | counter; histogram | `outcome`: `success`, `partial_failure`, `failed`, `aborted`, `timed_out`, `abandoned` |
| `ironcrew_tasks_total`; `ironcrew_task_duration_seconds` | counter; histogram | `outcome`: `success`, `error`, `skipped`, `cancelled` |
| `ironcrew_tool_calls_total`; `ironcrew_tool_call_duration_seconds` | counter; histogram | `outcome`: `success`, `error`, `cancelled` |
| `ironcrew_provider_requests_total`; `ironcrew_provider_request_duration_seconds` | counter; histogram | `provider`: `openai`, `openai_responses`, `anthropic`, `other`; `operation`: `chat`, `chat_with_tools`, `chat_stream`; `outcome`: `success`, `error`, `cancelled` |
| `ironcrew_provider_tokens_total` | counter | `provider`: `openai`, `openai_responses`, `anthropic`, `other`; `type`: `prompt`, `completion`, `cached` |
| `ironcrew_sse_connections_total` | counter | `scope`: `run_process`, `run_shared`, `conversation_process`; `outcome`: `accepted`, `limited` |
| `ironcrew_lease_losses_total` | counter | `scope`: `run`, `conversation` |
| `ironcrew_reconciliation_cycles_total` | counter | `outcome`: `success`, `error` |
| `ironcrew_reconciliation_records_total` | counter | none |
| `ironcrew_terminal_persistence_total` | counter | `scope`: `run_record`, `run_idempotency`, `run_indeterminate`, `conversation_commit`, `conversation_indeterminate`; `outcome`: `success`, `error`, `fenced` |
| `ironcrew_store_failures_total` | counter | `operation`: `metrics_snapshot`, `readiness`, `maintenance_heartbeat`, `reconciliation`, `lease_heartbeat`, `terminal_persistence`, `event_append`, `event_read`, `audit`, `run`, `idempotency`, `conversation`, `human_input` |

Every fixed combination is emitted even when its value is zero. The duration
histograms use cumulative second buckets `0.005`, `0.01`, `0.025`, `0.05`,
`0.1`, `0.25`, `0.5`, `1`, `2.5`, `5`, `10`, `30`, `60`, `120`, `300`, and
`+Inf`, plus `_sum` and `_count`. Reconciliation can count an abandoned run
without a trustworthy duration, so the matching run counter can exceed the
histogram count. Skipped tasks contribute a zero-second sample. Provider tokens
are added only from successful responses that report usage; they are not an
invoice, price, retry, or billing signal.

All of these counters and histograms are in-memory and per process. They use
non-blocking saturating atomic updates, are not persisted, and reset at process
start. A dropped in-flight task attempt or instrumented tool/provider future
records the closed `cancelled` outcome without putting an error or caller value
into a label. Store-failure counts cover explicitly instrumented operation
failures; they do not replace database-server, network, or platform telemetry.

The resource-acceptance families below are unlabeled, fixed-cardinality, and
describe only the process behind one scrape target:

| Series | Exact meaning |
|---|---|
| `ironcrew_process_memory_measurement_available` | `1` when Linux `/proc/self/status` supplied both `VmRSS` and `VmHWM`; otherwise `0`. |
| `ironcrew_process_resident_memory_bytes` | Current Linux `VmRSS` for the IronCrew process. Omitted when the measurement is unavailable. |
| `ironcrew_process_peak_resident_memory_bytes` | Linux `VmHWM` for the IronCrew process since startup. Omitted when the measurement is unavailable. |
| `ironcrew_postgres_pool_open_connections` | Connections currently open in this process's SQLx PostgreSQL pool. Omitted for JSON and SQLite stores. |
| `ironcrew_postgres_pool_in_use_connections` | Open SQLx pool connections currently checked out by this process. |
| `ironcrew_postgres_pool_connections_limit` | Configured maximum for this process's SQLx pool. |
| `ironcrew_process_active_provider_calls` | Logical provider futures currently active across run and conversation paths, including time spent in provider-instance pacing. |
| `ironcrew_process_peak_active_provider_calls` | Peak concurrent logical provider futures in this process since startup. This is measurement, not a provider semaphore or quota. |
| `ironcrew_process_eventbus_instances` | Underlying process-local EventBus replay buffers currently registered, including a terminal run buffer during its bounded late-replay window. EventBus clones share one registration. |
| `ironcrew_process_eventbus_retained_events` | Events currently retained across those replay buffers. |
| `ironcrew_process_eventbus_retained_bytes` | Approximate serialized bytes currently retained across those replay buffers; not allocator or RSS usage. |
| `ironcrew_process_eventbus_retained_events_capacity` | Sum of configured event-count capacities across the registered replay buffers. |
| `ironcrew_process_eventbus_retained_bytes_capacity` | Sum of configured byte capacities across the registered replay buffers. |
| `ironcrew_process_active_sse_connections` | Run and conversation SSE connections currently admitted by this process. |
| `ironcrew_process_active_sse_connections_limit` | Process-local SSE permit limit. |

The `/proc/self/status` read is Linux-only, capped at 64 KiB, fail-soft, and
coalesced for one second, including unavailable results. It deliberately does
not represent cgroup/container current usage or limits, OOM events, child/MCP
processes, or sidecars. Collect those from Railway/OpenShift/Kubernetes and the
container runtime. Likewise, SQLx pool gauges do not describe PostgreSQL
server memory, server-wide connection capacity, or other clients, and provider
gauges do not describe provider-side acceptance, retries, billing, or a
cluster-wide quota. Those remain external platform, database, and trusted
gateway/provider telemetry.

Store reads are coalesced for one second. The durable series expose global
record/response-byte usage and limits, global in-flight count, maximum usage by
any one principal, per-principal limits, principal count, and counts at
80/90/100 percent saturation. Labels are fixed; principal names, tokens, keys,
flow names, and other caller-controlled values are deliberately omitted.

If the store snapshot fails, `/metrics` returns `503` rather than serving
fabricated or stale durable utilization. That failed request records
`ironcrew_store_failures_total{operation="metrics_snapshot"}` for a later
successful scrape.

#### Scraping and aggregation

Scrape with a dedicated bearer principal used only by the monitoring client.
Preserve pod/instance target identity in the scraper rather than adding it to
IronCrew's metric labels, and deduplicate targets before aggregation.

- Sum `rate()` or `increase()` of the process-local counters across unique pod
  targets. Sum current per-process resource use and configured limits only when
  calculating fleet capacity; keep each process peak as a target-local
  high-water mark.
- Aggregate histogram bucket rates by `le` plus the dimensions being retained
  before calling `histogram_quantile`. For example, provider success latency is
  `histogram_quantile(0.95, sum by (le, provider, operation) (rate(ironcrew_provider_request_duration_seconds_bucket{outcome="success"}[10m])))`.
- Do not sum health booleans. Evaluate maintenance health per target or use the
  minimum to find any unhealthy process. Group the lifecycle one-hot gauge by
  `state`; sum degraded finalizers only for an impact total.
- The store-backed `ironcrew_idempotency_*` utilization snapshot represents the
  same shared PostgreSQL table prefix on every matching replica. Use one target
  or `max`, not `sum`, after verifying configuration parity.

Railway or another load balancer can route repeated public scrapes to the same
healthy replica. Use a private/platform integration that discovers and scrapes
each instance before claiming fleet coverage. On OpenShift, use authenticated
per-pod discovery rather than treating a Service-level scrape as proof that
every pod was observed. The baseline NetworkPolicy admits only ingress-controller
traffic: either scrape through authenticated per-pod Routes, or add a narrow
rule for the actual monitoring namespace/pod labels and TCP 8080. Do not expose
`/metrics` through an unauthenticated ServiceMonitor, public exception, or
cluster-wide ingress allowance.

#### Initial alert set

Start with a small set tied to an operator action, then tune windows and
thresholds from measured traffic:

1. **Target/readiness:** alert separately on a missing scrape target, and on
   `max_over_time(ironcrew_store_maintenance_healthy[2m]) == 0` or
   `min_over_time(ironcrew_process_terminal_persistence_degraded_finalizers[2m]) > 0`.
   Correlate a non-`accepting` lifecycle state with planned deployment events.
2. **Durability:** alert on increases in `ironcrew_lease_losses_total`,
   `ironcrew_terminal_persistence_total{outcome="error"}`, or
   `ironcrew_store_failures_total`, grouped by their fixed `scope`/`operation`.
   Starting five-minute expressions are
   `sum by (scope) (increase(ironcrew_lease_losses_total[5m])) > 0`,
   `sum by (scope) (increase(ironcrew_terminal_persistence_total{outcome="error"}[5m])) > 0`,
   and `sum by (operation) (increase(ironcrew_store_failures_total[5m])) > 0`.
   Page on sustained terminal/lease failures; route isolated recoverable
   operation failures to investigation rather than paging every increment.
3. **Provider quality:** alert on a sustained provider error ratio grouped by
   `provider,operation` only after a minimum request volume, and use the success
   histogram for a separately tuned p95 latency threshold. Treat cancellations
   as their own signal rather than silently folding them into provider errors.
4. **Capacity:** warn before a pod reaches active run/conversation/SSE limits,
   on sustained admission `limited` outcomes, and at the existing durable
   idempotency 80/90/100-percent thresholds.

Structured tracing remains available for Loki/Promtail or similar pipelines.
IronCrew does not ship or operate a hosted telemetry backend, dashboard,
long-term metrics store, or billing system. It deliberately omits
high-cardinality run/principal/flow/task/tool/error/URL labels. Container/cgroup,
PostgreSQL-server, provider-side, and platform measurements remain the
responsibility of Railway, OpenShift/Kubernetes, the database, and the provider.

---

## Kubernetes recipe

### Single-executor Deployment

Use one replica and `Recreate`. Kubernetes documents that `Recreate` kills the
existing pod before creating the replacement during a Deployment upgrade. Do
not add an HPA for a general-purpose service: scale only an application whose
routed requests fit the documented PostgreSQL run-SSE/keyed-control slice and
which does not require conversation routing or execution takeover.

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: ironcrew
spec:
  replicas: 1
  strategy:
    type: Recreate
  selector:
    matchLabels: { app: ironcrew }
  template:
    metadata:
      labels: { app: ironcrew }
    spec:
      automountServiceAccountToken: false
      terminationGracePeriodSeconds: 45
      containers:
      - name: ironcrew
        image: docker.io/skitsanos/ironcrew:2.22.0
        args: ["serve", "--host", "0.0.0.0", "--port", "8080", "--flows-dir", "/flows"]
        ports:
        - containerPort: 8080
        env:
        - name: IRONCREW_LOG
          value: "info"
        - name: IRONCREW_STORE
          value: "postgres"
        - name: DATABASE_URL
          valueFrom: { secretKeyRef: { name: ironcrew-secrets, key: database-url } }
        - name: OPENAI_API_KEY
          valueFrom: { secretKeyRef: { name: ironcrew-secrets, key: openai-api-key } }
        - name: IRONCREW_API_TOKEN
          valueFrom: { secretKeyRef: { name: ironcrew-secrets, key: api-token } }
        - name: IRONCREW_CORS_ORIGINS
          value: "https://app.example.com"
        - name: IRONCREW_MAX_RUN_LIFETIME
          value: "300"
        - name: IRONCREW_MAX_ACTIVE_RUNS
          value: "2"
        - name: IRONCREW_MAX_ACTIVE_CONVERSATIONS
          value: "4"
        - name: IRONCREW_FILE_WRITE_ROOT
          value: "/tmp/ironcrew-outputs"
        - name: IRONCREW_DEFAULT_MAX_CONCURRENT
          value: "2"
        - name: IRONCREW_SHUTDOWN_ROUTING_GRACE_SECS
          value: "5"
        - name: IRONCREW_SHUTDOWN_TIMEOUT_SECS
          value: "30"
        - name: IRONCREW_SHUTDOWN_DRAIN_MS
          value: "1000"
        - name: IRONCREW_MCP_ALLOWED_COMMANDS
          value: "__disabled__"
        - name: IRONCREW_MCP_ALLOWED_HTTP_HOSTS
          value: "__disabled__"
        resources:
          requests: { cpu: "250m", memory: "256Mi" }
          limits:   { cpu: "1",    memory: "1Gi"  }
        startupProbe:
          httpGet: { path: /health/ready, port: 8080 }
          periodSeconds: 5
          failureThreshold: 60
        readinessProbe:
          httpGet: { path: /health/ready, port: 8080 }
          periodSeconds: 5
        livenessProbe:
          httpGet: { path: /health/live, port: 8080 }
          periodSeconds: 15
        volumeMounts:
        - name: flows
          mountPath: /flows
          readOnly: true
      volumes:
      - name: flows
        configMap: { name: ironcrew-flows }
---
apiVersion: v1
kind: Service
metadata: { name: ironcrew }
spec:
  selector: { app: ironcrew }
  ports: [{ port: 80, targetPort: 8080 }]
```

The startup probe above allows 300 seconds, but that does not cover the
worst-case default PostgreSQL retry envelope. Ten retries after the initial
attempt mean 11 connection attempts. If every attempt consumes the default
30-second timeout, the attempts take up to 330 seconds and the ten backoffs add
another 181 seconds (`1 + 2 + 4 + 8 + 16 + 5 * 30`), for approximately 511
seconds before the PostgreSQL version check and bootstrap begin. Bootstrap can
then wait on its advisory lock and database DDL without a separate bounded
timeout. Treat 300 seconds as a deployment budget: either increase the startup
probe or lower the retry, backoff, and connection-timeout settings so their
configured envelope fits with bootstrap headroom. Startup probes prevent
liveness/readiness checks from interfering during that window; readiness then
removes a pod from Service traffic during a later storage outage without
killing it.

### Flows as ConfigMap

For small flow sets, mount `crew.lua` / `config.lua` via ConfigMap. For larger sets, bake them into a separate image layer or pull from object storage at startup.

Do not configure session affinity as a substitute for this restriction.
Affinity can improve routing but cannot transfer run execution or an in-flight
conversation handle between processes. Configured PostgreSQL run SSE, a keyed
HITL mailbox, and keyed PostgreSQL conversation messages from the committed
IC-008 implementation work without affinity at committed turn boundaries, but
none can transfer a Lua VM or suspended coroutine. IC-008's affinity-free
OpenShift canary passed this
committed-boundary slice, but its dirty artifact was unpublished and Railway
remains unrun. PostgreSQL conversation SSE is unsupported; JSON/SQLite
conversation and run SSE remain process-local.

---

## OpenShift specifics

The checked-in [`deploy/openshift.yaml`](../deploy/openshift.yaml) is the
production baseline. It deliberately uses:

- one replica with Deployment strategy `Recreate` as the conservative
  unpublished-release baseline and for applications that require conversation
  SSE, in-flight conversation takeover, or unkeyed controls; the dated
  OpenShift IC-008 canary passed only the keyed committed-boundary slice
- `/health/ready` for startup and readiness, and `/health/live` for liveness
- readiness removes a pod from Service endpoints after a lease heartbeat or
  reconciliation failure; liveness deliberately does not restart it for that
  maintenance condition
- a 300-second startup-probe budget, which is shorter than the approximately
  511-second worst-case default connection-retry envelope and excludes
  bootstrap advisory-lock and DDL wait time; tune the probe and database retry
  settings together
- `restricted-v2`-compatible security settings: arbitrary non-root UID, no
  privilege escalation, all capabilities dropped, runtime-default seccomp, and
  a read-only root filesystem
- `emptyDir` mounted at `/tmp`, with flows mounted read-only from the
  `ironcrew-flows` ConfigMap
- PostgreSQL credentials and API tokens from the externally created
  `ironcrew-secrets` Secret; optional HITL keyring references use that Secret
- stdio MCP disabled by a non-matching command allowlist until the operator
  replaces it with the exact trusted binaries installed in the image
- a pod-selecting `NetworkPolicy` that admits Route traffic and permits only
  cluster DNS, same-namespace PostgreSQL on TCP 5432, and public HTTPS egress
- principal-scoped work/control/observation admission, idempotency budgets,
  bounded HITL metadata, a bounded PostgreSQL event journal, and a bounded
  conversation lifecycle registry, sized for the 1 GiB pod baseline
- an explicit 5000 ms journal-write attempt bound for managed PostgreSQL; its
  4-second database statement bound and each 15.65-second acknowledgement
  envelope remain bounded, while the 30-second teardown deadline stays
  authoritative if both pre-terminal and terminal waits hit their maxima
- a 5-second routing grace, 30-second stopping deadline, 1-second cleanup
  window, and 45-second pod grace (`5 + 30 + 1 + 5 = 41` seconds including
  the operator margin)

Apply it only after replacing the image tag, example CORS origin, ConfigMap,
and Secret names:

```bash
oc apply -f deploy/openshift.yaml
```

### Two-replica rolling overlay

Use a horizontal overlay only for an application whose routed surface fits the
documented PostgreSQL keyed-run/run-SSE slice. After the first drain-aware
version is homogeneous, a bounded two-replica rollout can use:

```yaml
spec:
  replicas: 2
  # Do not retire an old pod after a replacement's first transiently healthy
  # probe. Require a stable readiness window before it counts as available.
  minReadySeconds: 10
  strategy:
    type: RollingUpdate
    rollingUpdate:
      maxSurge: 1
      maxUnavailable: 0
```

At steady state the checked-in limits declare 4 active runs, 8 resident
conversations, 32 SSE connections, 4 pool connections, and 2 GiB of container
memory allocation. `maxSurge: 1` permits three non-terminating pods, but
terminating pods are not counted in that controller limit. For one controlled
two-replica rollout, conservatively budget old `R` + new `R` = 4 physical pods
until both old pods exit: 8 run slots, 16 resident conversations, 64 SSE
connections, 8 pool connections, and 4 GiB of platform memory allocation.
Overlapping rollouts or manual deletion can exceed even that envelope, so
monitor the actual pod count and do not begin another rollout while old pods
remain terminating. PostgreSQL-global ledger/journal caps remain one shared
budget; provider concurrency is still not cluster-global.

Kubernetes starts local termination while the control plane updates the
EndpointSlice. Terminating endpoints normally become `ready: false`, but
propagation through kube-proxy, Routes, external load balancers, and existing
connections is not instantaneous. Keep IronCrew's routing grace and verify the
actual cluster path. Do not set the container stop signal to `SIGUSR1`: that
signal drains without exiting, so kubelet would eventually need `SIGKILL`.
Leave termination as `SIGTERM`. A `preStop` hook runs inside the same 45-second
grace and therefore reduces the time available to IronCrew rather than adding
a second drain budget. Keep a non-zero `minReadySeconds` stabilization window:
without it, the Deployment controller may retire the old pod after one healthy
probe even if the replacement immediately becomes unready. IronCrew coalesces
overlapping readiness checks for up to one second so routine kubelet/operator
probe overlap does not itself create that flap.

The first deployment containing this fence is a mixed-version boundary. An old
owner cannot publish the fence and an old peer cannot honor it. Use the
one-replica `Recreate` baseline, a maintenance/scale-to-zero cutover, or an
externally verified zero-active-work transition for that first rollout. Enable
the rolling overlay only after every routable replica reports the new
`lifecycle_state` capability.

### Restricted SCC and non-root

OpenShift's default `restricted-v2` SCC uses a namespace-specific UID range.
Do not hard-code `runAsUser` in the workload manifest; the assigned UID is not
predictable across projects. The shipped image has a numeric non-root default
for ordinary Docker runtimes, while its writable directories are owned by
group `0` with group permissions matching the owner so OpenShift can substitute
an arbitrary UID safely.

- Keep writes in `/tmp` or an explicitly mounted PVC/`emptyDir`.
- Do not bind to ports below 1024.
- Do not grant a broader SCC just to run IronCrew.
- Leave `runAsNonRoot`, `allowPrivilegeEscalation: false`, dropped capabilities,
  and `RuntimeDefault` seccomp in place.

Port `8080` in the manifest is non-privileged. IronCrew requires no root
privileges at runtime.

### Flow and memory writes

The baseline mounts flows read-only. Flows that use IronCrew's file-backed crew
memory need a separate writable volume at the relevant flow path; PostgreSQL
run/session storage does not replace that memory file. Do not make the whole
container root filesystem writable to accommodate one flow.

### Routes instead of Ingress

The checked-in manifest includes a `Route` pointing at the `ironcrew` Service,
with edge TLS termination and HTTP-to-HTTPS redirect. Replace or remove it when
the cluster uses a different ingress policy.

### NetworkPolicy and egress

The checked-in `NetworkPolicy` selects only IronCrew pods; it does not impose a
namespace-wide default deny on PostgreSQL or other workloads. Its ingress rule
admits TCP 8080 from namespaces carrying OpenShift's preferred
`policy-group.network.openshift.io/ingress` label, its legacy
`network.openshift.io/policy-group=ingress` label, or the default
`openshift-ingress` namespace name. Verify the installed ingress controller's
namespace labels before rollout, especially when using a custom router. Test
the Route and kubelet probes after applying the policy because node-to-pod
probe handling is CNI-specific.

Egress is allowlisted to:

- UDP/TCP 53 and 5353 in `openshift-dns` (the latter covers OVN-Kubernetes
  post-DNAT policy evaluation), plus UDP/TCP 53 in `kube-system` for
  CoreDNS-compatible clusters;
- TCP 5432 to pods in the IronCrew namespace; and
- TCP 443 to public IPv4/IPv6 addresses, excluding private, loopback,
  link-local, reserved/documentation, benchmarking, and multicast ranges.

Plain outbound HTTP is intentionally denied, including redirect hops to port
80. Use HTTPS provider/tool URLs; add port 80 only for an explicitly reviewed
dependency.

This baseline assumes PostgreSQL is in the same namespace. For an
operator-managed database in another namespace, replace the empty PostgreSQL
`podSelector` with a stable database pod label and a tightly matched
`namespaceSelector`; do not open the entire private CIDR. Likewise, if cluster
DNS runs under another namespace label, update only the DNS peers. NetworkPolicy
is additive, so an operator may add a separate narrow rule for an internal MCP
service or a private provider endpoint without weakening public HTTPS rules.
Restrict RBAC permission to create additional NetworkPolicy objects in the
production namespace, because another allow policy selecting IronCrew pods is
unioned with this one.

Kubernetes `ipBlock` behavior around Service translation and node traffic can
vary by network plugin. Validate resolved destinations from inside a staged pod:
public provider HTTPS and the configured database must work, while requests to
loopback, link-local metadata addresses, RFC1918 destinations, and unrelated
same-namespace ports must fail. IronCrew's connect-time SSRF checks remain a
second layer and must not be disabled merely because NetworkPolicy is present.

### Secrets

OpenShift `Secret` objects work the same as Kubernetes ones. For stricter environments, use **SealedSecrets** or the platform's vault integration.

---

## Railway recipe

Railway has no Kubernetes manifests — everything is a service, an environment variable, or a config file in the repo.

### 1. Deploy the checked-in configuration

Connect the repository at its root. [`railway.json`](../railway.json) selects the
root Dockerfile and configures:

- exactly one replica
- `/health/ready` as the deployment healthcheck, with a 300-second timeout
- a per-replica limit of 1 vCPU and 1 GiB through
  `deploy.limitOverride.containers`
- no extra *accepting* overlap (`overlapSeconds: 0`); old and new allocations
  can still coexist while the old deployment handles SIGTERM drain
- a 30-second SIGTERM draining window
- restart policy `ALWAYS`

Do not override the image command unless the flows live elsewhere. The image
starts `ironcrew serve --flows-dir /flows` itself.

The base image creates `/flows` but intentionally does not bundle a production
flow. Before deploying, either add `COPY --chown=10001:0 flows/ /flows/` to a
service-specific image or mount and initialize a Railway volume at `/flows`.
An empty directory is storage-readable, so `/health/ready` can succeed even
when no deployable flow has been supplied; call
`GET /flows/{expected-flow}/validate` during rollout.

IronCrew natively reads Railway's injected `PORT`: CLI `--port` wins first,
then `IRONCREW_PORT`, then `PORT`, then `3000`. When `PORT` is present and no
host override is supplied, the server binds `0.0.0.0`. Invalid environment port
values fail startup instead of silently binding a different port.

### 2. Environment variables

In the Railway service settings, add:

```
IRONCREW_LOG=info
IRONCREW_API_TOKEN=<generated token>
IRONCREW_CORS_ORIGINS=https://your-frontend.example.com
IRONCREW_MAX_RUN_LIFETIME=300
IRONCREW_REQUIRE_IDEMPOTENCY_KEY=true
IRONCREW_IDEMPOTENCY_TTL_SECONDS=86400
IRONCREW_IDEMPOTENCY_MAX_RECORDS=10000
IRONCREW_IDEMPOTENCY_MAX_RECORDS_PER_PRINCIPAL=2500
IRONCREW_IDEMPOTENCY_MAX_IN_FLIGHT_PER_PRINCIPAL=16
IRONCREW_IDEMPOTENCY_PRUNE_BATCH=1000
IRONCREW_IDEMPOTENCY_MAX_RESPONSE_BYTES=4194304
IRONCREW_IDEMPOTENCY_MAX_TOTAL_RESPONSE_BYTES=67108864
IRONCREW_IDEMPOTENCY_MAX_TOTAL_RESPONSE_BYTES_PER_PRINCIPAL=16777216
IRONCREW_ADMISSION_WORK_RATE_PER_MINUTE=60
IRONCREW_ADMISSION_WORK_BURST=10
IRONCREW_ADMISSION_CONTROL_RATE_PER_MINUTE=120
IRONCREW_ADMISSION_CONTROL_BURST=20
IRONCREW_ADMISSION_OBSERVATION_RATE_PER_MINUTE=600
IRONCREW_ADMISSION_OBSERVATION_BURST=20
IRONCREW_MAX_ACTIVE_RUNS=2
IRONCREW_MAX_ACTIVE_CONVERSATIONS=4
IRONCREW_MAX_CONVERSATION_LIFECYCLES=256
IRONCREW_CHAT_SESSION_IDLE_SECS=600
IRONCREW_DEFAULT_MAX_CONCURRENT=2
IRONCREW_MAX_CONCURRENT_TASKS=4
IRONCREW_CREW_GOAL_MAX_BYTES=65536
IRONCREW_MAX_APPROVAL_PATTERNS=64
IRONCREW_MAX_MEMORY_ITEMS=2000
IRONCREW_MAX_MEMORY_TOKENS=200000
IRONCREW_MAX_SERVER_TOOLS=8
IRONCREW_MAX_VECTOR_STORE_IDS=16
IRONCREW_MAX_MODEL_ROUTES=32
IRONCREW_LUA_MAX_MEMORY_BYTES=25165824
IRONCREW_LUA_MAX_EXECUTION_SECONDS=900
IRONCREW_MAX_BODY_SIZE=8388608
IRONCREW_HTTP_MAX_REQUEST_HEADER_BYTES=65536
IRONCREW_HTTP_MAX_REQUEST_BODY_BYTES=4194304
IRONCREW_HTTP_MAX_RESPONSE_BYTES=4194304
IRONCREW_HTTP_MAX_OUTPUT_BYTES=8388608
IRONCREW_PROVIDER_MAX_OUTPUT_BYTES=4194304
IRONCREW_PROVIDER_MAX_STREAM_BYTES=8388608
IRONCREW_CHAT_HISTORY_MAX_BYTES=8388608
IRONCREW_MAX_REASONING_BYTES=524288
IRONCREW_MAX_IMAGE_BYTES=5242880
IRONCREW_FOREACH_MAX_ITEMS=50
IRONCREW_FOREACH_MAX_OUTPUT_BYTES=4194304
IRONCREW_TASK_RESULT_MAX_OUTPUT_BYTES=4194304
IRONCREW_TASK_RESULT_MAX_REASONING_BYTES=2097152
IRONCREW_RUN_RESULTS_MAX_BYTES=16777216
IRONCREW_MAX_EVENTS=200
IRONCREW_EVENT_REPLAY_MAX_BYTES=1048576
IRONCREW_EVENT_MAX_BYTES=131072
IRONCREW_EVENT_CHANNEL_CAPACITY=8
IRONCREW_EVENT_JOURNAL_RETENTION_SECS=3600
IRONCREW_EVENT_JOURNAL_MAX_TOTAL_EVENTS=10000
IRONCREW_EVENT_JOURNAL_MAX_TOTAL_BYTES=67108864
IRONCREW_EVENT_JOURNAL_PAGE_MAX_BYTES=262144
IRONCREW_EVENT_JOURNAL_POLL_INTERVAL_MS=1000
IRONCREW_EVENT_JOURNAL_READ_TIMEOUT_MS=2000
IRONCREW_EVENT_JOURNAL_WRITE_TIMEOUT_MS=5000
IRONCREW_EVENT_JOURNAL_PRUNE_BATCH=500
IRONCREW_MAX_SSE_CONNECTIONS=16
IRONCREW_ASK_HUMAN_MAX_PENDING=8
IRONCREW_ASK_HUMAN_MAX_PENDING_BYTES=524288
IRONCREW_HITL_POLL_INTERVAL_MS=1000
IRONCREW_HITL_READ_TIMEOUT_MS=2000
IRONCREW_HITL_PG_MAX_CONCURRENT_READS=2
IRONCREW_DB_POOL_SIZE=2
IRONCREW_SHUTDOWN_ROUTING_GRACE_SECS=5
IRONCREW_SHUTDOWN_TIMEOUT_SECS=15
IRONCREW_SHUTDOWN_DRAIN_MS=1000
IRONCREW_STORE=postgres
IRONCREW_FILE_WRITE_ROOT=/data/outputs
IRONCREW_MCP_ALLOWED_COMMANDS=__disabled__
IRONCREW_MCP_ALLOWED_HTTP_HOSTS=__disabled__
OPENAI_API_KEY=sk-...
```

When enabling shared HITL, add `IRONCREW_HITL_ENCRYPTION_KEYS` and
`IRONCREW_HITL_ACTIVE_KEY_ID` as sealed Railway variables rather than placing
their values in this plaintext recipe. Each rotation phase requires a new
deployment because IronCrew reads them once at process startup. Follow the
[three-revision rotation gate](#hitl-key-rotation-on-railway-and-openshift)
before removing a key.

Railway's Postgres add-on auto-injects `DATABASE_URL`.

Railway injects `RAILWAY_REPLICA_ID` only into each running replica. The
service-variable expression
`IRONCREW_INSTANCE_ID=${{RAILWAY_REPLICA_ID}}` was observed to resolve to an
empty configured value during the 2026-08-10 canary, which IronCrew correctly
rejects. Leave `IRONCREW_INSTANCE_ID` absent: IronCrew now validates and uses
the non-empty runtime `RAILWAY_REPLICA_ID` automatically. It generates a
process-lifetime fallback only when neither identity is present. This keeps
authenticated IronCrew response attribution aligned with Railway's runtime
replica identifier without relying on service-variable interpolation. Railway
may reuse that replica identifier after an in-place restart; use the separate
capability `process_start_id` and platform lifecycle evidence to distinguish
the old process from its replacement.

For multiple callers, migrate the shared `IRONCREW_API_TOKEN` to a secret
`IRONCREW_API_TOKENS` JSON object, for example
`{"frontend":"<token>","automation":"<token>"}`. Keep the legacy token
configured until outstanding retries have aged past the longest idempotency
TTL, as described in the security section above. Railway variables are still
secrets: do not commit real token JSON to this file or the repository.

With `IRONCREW_REQUIRE_IDEMPOTENCY_KEY=true`, every caller of the run and
conversation-message mutation endpoints must generate a stable 1–128 byte
visible-ASCII key per logical operation. Keep the key across client/proxy
timeouts and change it only for an intentional new operation. See
[REST API safe retries](rest-api.md#safe-retries-with-idempotency-key).

Railway Pro provides a much higher account resource ceiling than the smaller
plans, but that ceiling includes replica multiplication and is not a sizing
recommendation for this service. Start with an explicit per-service allocation
around 1 vCPU/1 GiB and the conservative caps above (the checked-in OpenShift
manifest contains the fuller set). Raise CPU/RAM and application caps only
after a representative container soak test. The checked-in general-purpose
baseline keeps `numReplicas: 1`; Pro plan headroom does not make owner-local
execution horizontally safe. The keyed PostgreSQL conversation-message path
passes its local two-process gate and OpenShift canary but has no Railway
IC-008 canary or published artifact. Scale only applications whose routed
surfaces fit the documented PostgreSQL run-SSE/keyed-control contract, and
include per-replica pools, admission, Lua memory, journal queues/pages, and
HITL polling in the aggregate budget.

During replacement Railway can run both the old `R`-replica deployment and the
new `R`-replica deployment while SIGTERM drain completes, so the physical
planning count is `P = 2R`. At `R = 2`, the checked-in per-replica limits imply
8 active-run slots, 16 resident conversation slots, 64 SSE slots, 8 maximum
pool connections, and 4 GiB of container allocation. `overlapSeconds: 0`
removes a deliberate accepting delay; it does not eliminate this bounded
teardown overlap. PostgreSQL-global caps still remain one shared budget.

The live Railway JSON schema supports
`deploy.limitOverride.containers.{cpu,memoryBytes}`. The checked-in config uses
that contract to cap each replica at **1 vCPU / 1 GiB**; config-as-code overrides
the equivalent dashboard values for that deployment. Verify the effective
values in deployment details after rollout and keep the dashboard's Replica
Limits aligned as defense against deployments that bypass this repository
config. See Railway's [config-as-code reference](https://docs.railway.com/config-as-code/reference)
and [cost-control guide](https://docs.railway.com/pricing/cost-control#replica-limits).

Railway's config schema does not provide a destination NetworkPolicy or an
outbound CIDR/hostname allowlist. Its `ipv6EgressEnabled` switch controls the
address family, not destinations, and the checked-in config deliberately leaves
it at the platform default: legacy `*.railway.internal` environments can be
IPv6-only, including the private PostgreSQL address. Keep IronCrew's SSRF
protection and MCP host allowlist enabled, expose only the authenticated HTTPS
service, and use an external network boundary when compliance requires
platform-enforced egress. Because protected IronCrew clients intentionally
ignore proxy environment variables, a mandatory HTTP egress proxy is not a
supported substitute today. See Railway's
[private-networking contract](https://docs.railway.com/private-networking).

### 3. Health check

The checked-in healthcheck path is `/health/ready`. Railway waits for HTTP 200
before making a new deployment active, and uses the injected `PORT` for that
request. Railway healthchecks are deployment gates, **not continuous runtime
monitoring**. A later lease-maintenance failure still changes the endpoint to
`503`, but Railway does not continuously use that result to withdraw the active
replica; use an external monitor for ongoing availability and alert on
`component: "storage_maintenance"`.

### 4. SIGTERM grace

Railway's default draining time is zero seconds. `railway.json` explicitly sets
30 seconds between SIGTERM and SIGKILL, so keep IronCrew's own deadline inside
that window:

```
IRONCREW_SHUTDOWN_ROUTING_GRACE_SECS=5
IRONCREW_SHUTDOWN_TIMEOUT_SECS=15
IRONCREW_SHUTDOWN_DRAIN_MS=1000
```

This budgets `5 + 15 + 1 + 5 = 26` seconds, including a five-second operator
margin, inside Railway's 30-second teardown window when the durable fence
commits during the routing grace. Railway sends `SIGTERM` and, on expiry,
`SIGKILL`; it does not send `SIGUSR1`. The `SIGTERM` path therefore includes
IronCrew's fence and routing grace automatically. If fencing cannot commit,
Railway may use `SIGKILL` and the run later reconciles as `abandoned`.

`overlapSeconds: 0` prevents extra overlap after the candidate becomes active;
it does not make owner-local live controls distributed. Multiple replicas can
use PostgreSQL run SSE and the keyed cancellation/HITL slice. The committed
IC-008 implementation also supports arbitrary routing of keyed PostgreSQL
messages between committed conversation turns. Its local and OpenShift gates
pass, but Railway has not run the IC-008 canary and no published release
contains this implementation.
Keep the service at one when clients require conversation SSE, in-flight turn
takeover, unkeyed live controls, or any other owner-local surface.

For the first drain-aware release, do not infer mixed-version safety from this
timing. Use a maintenance/zero-active-work cutover or one-replica replacement,
then verify every routed response reports the new `lifecycle_state` capability
before enabling a two-replica rolling deployment.

### 5. Volumes

For the HTTP service, use Railway PostgreSQL instead of JSON/SQLite. If a flow
uses file-backed crew memory, mount and back up a volume for that specific
writable path; run/session PostgreSQL storage does not persist the separate
memory file.

---

## Building container images

### Source Dockerfile

The root [`Dockerfile`](../Dockerfile) uses the exact Rust `1.97.1` builder that
matches `Cargo.toml`'s minimum supported Rust version, builds with
`cargo build --release --locked`, and copies the executable into
`debian:13-slim`. The runtime is intentionally glibc-based and dynamically
linked; it is not distroless, musl, or `scratch`.

The runtime stage:

- installs only CA certificates
- runs as numeric non-root UID `10001`, group `0`
- remains compatible with an OpenShift-assigned arbitrary UID
- exposes the local/container default port `3000`
- supplies a runnable server `CMD`

Release publishing uses [`docker/runtime.Dockerfile`](../docker/runtime.Dockerfile)
with GNU/Linux artifacts built by the release workflow using Rust `1.97.1` and
`--locked`. The exact tag workflow assembles one `linux/amd64` plus
`linux/arm64` OCI archive on a content-addressed Wolfi base index, records its
source and OCI object hashes in a signed receipt, and publishes both as release
assets. The separately authorized Docker workflow verifies and promotes that
archive; it does not rebuild a historical release from the current default
branch. The release image retains the source image's permissions, numeric user,
environment, and command contract.

This promotion protocol is resolved under [IC-015](issues/IC-015.md). On
2026-08-12, Docker Hub's production repository was changed to the exact
stable-semver-only immutability rule while leaving `latest` mutable. A
disposable repository passed the real
replay/conflict/concurrent-`latest` protocol and was then removed exactly; the
retained machine receipt is linked from IC-015. No production image or tag was
published or moved. The exact harness/evidence commit passed all platform CI
jobs. Production Docker publication remains deferred under IC-014's
release-control gate and the user's release-last sequence.
The protocol starts with the first release produced by the new tag workflow;
it does not retroactively rebuild or promote legacy releases that lack the
signed OCI archive and receipt.

The non-production IC-015 registry gate is
[`scripts/dockerhub_promotion_acceptance.py`](../scripts/dockerhub_promotion_acceptance.py).
It derives the only permitted target as
`<namespace>/ironcrew-ic015-acceptance-<run-id>`; callers cannot pass the
production repository. Its `prepare` phase creates that exact public disposable
repository, installs the canonical semantic-version immutability rule, and
records its name, description, registration timestamp, and fingerprint. The
`run` phase requires that bound receipt, an empty tag inventory, two distinct
public source images pinned by digest, an authenticated `skopeo` session, and
explicit promotion authorization. It exercises initial and identical version
promotion, protocol conflict refusal, a direct registry overwrite rejection,
a same-archive mutable-tag positive control, a second version, and deterministic `latest` repair when the injected stable
release changes between reads. It revalidates the external immutability rule
after the rejected overwrite and requires the exact final tag set.

Use a unique ID and new evidence paths; the helper refuses to overwrite an
existing receipt:

```bash
python3 -B scripts/dockerhub_promotion_acceptance.py prepare \
  --namespace skitsanos --run-id 20260812t120000z-deadbeef \
  --authorize-create --evidence /secure/path/ic015-prepare.json
python3 -B scripts/dockerhub_promotion_acceptance.py run \
  --namespace skitsanos --run-id 20260812t120000z-deadbeef \
  --input-evidence /secure/path/ic015-prepare.json \
  --source-a docker://example/image@sha256:<64-lowercase-hex> \
  --source-b docker://example/other@sha256:<64-lowercase-hex> \
  --authorize-promotion --evidence /secure/path/ic015-run.json
```

Supply `DOCKERHUB_USERNAME` and `DOCKERHUB_TOKEN` through the environment and
authenticate `skopeo` without placing the token in command history. After the
receipt passes, delete only the exact disposable repository in Docker Hub's
authenticated UI, then bind absence evidence to the run receipt:

```bash
python3 -B scripts/dockerhub_promotion_acceptance.py verify-cleanup \
  --namespace skitsanos --run-id 20260812t120000z-deadbeef \
  --input-evidence /secure/path/ic015-run.json \
  --evidence /secure/path/ic015-cleanup.json
```

Repository deletion is destructive and removes its images permanently. The
cleanup phase is read-only: it passes only after the authenticated API reports
that exact bound repository absent and `skopeo` cannot resolve any of its three
acceptance tags. The dated IC-015 receipt records one completed run of this
procedure. Production publication remains deferred under IC-014's separate
release-control gate and the user's release-last sequence.

### Excluding MCP

If you don't need MCP or PostgreSQL, build with `--no-default-features`. If you
still need PostgreSQL, re-enable it explicitly:

```
RUN cargo build --release --locked --no-default-features --features postgres
```

### Official platform references

- [Railway config as code](https://docs.railway.com/config-as-code/reference)
- [Railway healthchecks](https://docs.railway.com/deployments/healthchecks)
- [Railway deployment teardown](https://docs.railway.com/deployments/deployment-teardown)
- [Railway plan resource ceilings](https://docs.railway.com/pricing/plans)
- [Railway-provided variables](https://docs.railway.com/variables/reference)
- [Kubernetes Deployment strategies](https://kubernetes.io/docs/concepts/workloads/controllers/deployment/)
- [Kubernetes pod termination lifecycle](https://kubernetes.io/docs/concepts/workloads/pods/pod-lifecycle/)
- [Kubernetes EndpointSlice conditions](https://kubernetes.io/docs/concepts/services-networking/endpoint-slices/)
- [Kubernetes startup, readiness, and liveness probes](https://kubernetes.io/docs/tasks/configure-pod-container/configure-liveness-readiness-startup-probes/)
- [Kubernetes NetworkPolicy](https://kubernetes.io/docs/concepts/services-networking/network-policies/)
- [OpenShift NetworkPolicy](https://docs.redhat.com/en/documentation/openshift_container_platform/4.22/html/network_security/network-policy)
- [OpenShift `restricted-v2` SCC behavior](https://docs.redhat.com/en/documentation/openshift_container_platform/4.22/html/authentication_and_authorization/managing-pod-security-policies)

---

## Troubleshooting

### Pod killed by OOM
- Lower `IRONCREW_DEFAULT_MAX_CONCURRENT` and `IRONCREW_MAX_EVENTS`.
- Check per-run EventBus replay, PostgreSQL producer queues/reader pages, HITL
  pending metadata, and the number of concurrent SSE connections across pods.
- Reduce `IRONCREW_MAX_PROMPT_CHARS` and per-tool byte caps.

### MCP stdio children orphaned after SIGKILL
- `terminationGracePeriodSeconds` was too short. Raise it so routing grace,
  stopping timeout, background cleanup, any `preStop`, and an operator margin
  all fit the one pod-termination budget.

### `/health/live` passes but readiness is 503
- The process is alive but a readiness gate failed. `component: "lifecycle"`
  with `lifecycle_state` means this replica was deliberately withdrawn and
  cannot be made accepting without restart. `component: "storage"` points to
  the ordinary store probe. `component: "storage_finalization"` means a run or
  conversation finalizer is retrying durable persistence.
  `component: "storage_maintenance"` means startup or periodic lease
  heartbeat/reconciliation failed or timed out;
  inspect PostgreSQL connectivity, pool saturation, held advisory locks, and
  long transactions. Readiness recovers only after both maintenance operations
  succeed in one cycle. Provider APIs are not part of readiness.

### Railway deployment passes but later becomes unhealthy
- Railway's healthcheck is a deployment-time gate, not continuous monitoring. Add an external uptime monitor against `/health/ready`.

### CORS blocks legitimate frontend
- Set `IRONCREW_CORS_ORIGINS` explicitly — default is deny-all. Never use `*` in production.

### OpenShift Route or provider calls fail after NetworkPolicy
- Verify the router and DNS namespace labels used by this cluster, then inspect
  the resolved provider/database destination from inside the pod. Add a narrow
  namespace/pod/CIDR rule for the real dependency; do not remove egress policy
  or allow all RFC1918 space as a shortcut.

### Run records lost between deploys
- Using JSON or SQLite backend with an `emptyDir` volume? Switch to a persistent volume or PostgreSQL.

---

## Checklist before go-live

- [ ] `IRONCREW_API_TOKEN` set to a strong value
- [ ] Each production caller has its own `IRONCREW_API_TOKENS` principal where
      isolation is required; the token map remains in a platform Secret
- [ ] `IRONCREW_CORS_ORIGINS` restricted to your frontend domains
- [ ] `IRONCREW_ALLOW_SHELL` unset (unless sandboxed)
- [ ] `IRONCREW_MCP_ALLOWED_COMMANDS` whitelist set (if using MCP stdio)
- [ ] `IRONCREW_MCP_ALLOWED_HTTP_HOSTS` is `__disabled__` or lists only exact operator-trusted hosts
- [ ] `IRONCREW_MAX_RUN_LIFETIME` tuned to workload (accepted runs may continue
      through routing grace but are aborted at `stopping`, so the full run
      lifetime is not a termination-grace promise)
- [ ] `IRONCREW_REQUIRE_IDEMPOTENCY_KEY=true`; clients preserve keys across retries, and ledger byte/record caps fit the pod/database budget
- [ ] Per-principal idempotency and admission limits are set, and `429`
      saturation alerts are configured from protected `/metrics`
- [ ] `IRONCREW_SHUTDOWN_ROUTING_GRACE_SECS + IRONCREW_SHUTDOWN_TIMEOUT_SECS + IRONCREW_SHUTDOWN_DRAIN_MS/1000 + preStop + operator margin` fits within platform termination grace
- [ ] Alert on a replica stuck in `fencing`; the clean-shutdown arithmetic
      assumes PostgreSQL commits the owner fence inside routing grace, while a
      prolonged fence failure intentionally falls back to platform `SIGKILL`
      and later `abandoned` reconciliation
- [ ] Replica count matches the
  [multi-replica live-control contract](multi-replica.md); use exactly one
      until a published release contains the IC-008 behavior, on Railway until
      its attributed canary passes, or whenever clients require conversation
      SSE, in-flight conversation takeover, or unkeyed controls; PostgreSQL run
      SSE and keyed
      conversation messages have separate bounded contracts
- [ ] First drain-aware rollout avoids mixed old/new drain semantics; later
      rolling deployments verify every routable replica exposes
      `lifecycle_state`
- [ ] Rolling deployments use a measured non-zero `minReadySeconds` window and
      preserve at least one continuously ready endpoint during replacement
- [ ] Replacement/surge arithmetic uses maximum physical pods, including
      Railway old/new teardown (`2R`) and OpenShift terminating pods beyond
      the controller's `replicas + maxSurge` envelope
- [ ] Railway config-as-code and dashboard replica limits agree at the intended
      baseline (the checked-in start point is 1 vCPU / 1 GiB)
- [ ] PostgreSQL configured for production durability
- [ ] PostgreSQL event-journal logical limits, read/page/poll/prune bounds, and
      actual database/WAL/autovacuum footprint are monitored independently
- [ ] If cross-replica HITL is enabled, every replica receives the same readable
      `IRONCREW_HITL_ENCRYPTION_KEYS` set; steady state uses one active id, while
      rotation follows the ordered mixed-active phase and keeps old keys until
      both mailbox fingerprint columns contain zero old references
- [ ] Secrets mounted from `Secret` / vault, not baked into image
- [ ] Startup/readiness probes hit `/health/ready`; liveness hits `/health/live`
- [ ] Resource `requests` and `limits` set on the container
- [ ] OpenShift Route, DNS, PostgreSQL, and provider HTTPS verified with the
      NetworkPolicy applied; private/link-local negative probes fail
- [ ] Production image reference replaced with the signed release digest (`image@sha256:...`), not a mutable tag
- [ ] The complete deployment-evidence tuple is set for platform acceptance;
      every active process independently matches its revision, artifact, flow,
      effective-config, and HITL-keyring fingerprints
- [ ] `process_start_id` and platform inventory prove process replacement even
      when a stable platform replica id is reused; unique attribution fields and
      platform resource limits are recorded outside config-fingerprint equality
- [ ] Container runs non-root with no privilege escalation and dropped capabilities
- [ ] TLS terminated at ingress / router / load balancer
- [ ] Log level set to `info` or lower (never `debug` in prod)
