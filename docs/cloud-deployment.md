# Cloud Deployment

How to run IronCrew in managed cloud environments: **Kubernetes**, **OpenShift**, **Railway**, and similar platforms. This doc covers graceful shutdown, resource limits, security posture, and platform-specific recipes.

IronCrew is distributed as a single Rust executable. The default Linux release
and container image use the GNU target and are dynamically linked against
glibc; the Debian runtime image supplies that runtime environment. IronCrew
runs in `serve` mode as a long-lived HTTP server, or in `run` mode as a
one-shot job.

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
- Source and release container images use `debian:13-slim` plus CA certificates.
- The image defaults to numeric UID `10001` and group `0`; writable directories are group-writable so OpenShift can substitute its namespace-assigned UID.
- The image has a runnable `CMD` (`ironcrew serve --flows-dir /flows`) and listens on port `3000` unless an environment port overrides it.
- No systemd, no daemonization — runs in the foreground; logs to stderr.

---

## Graceful shutdown

IronCrew handles `SIGTERM` (Kubernetes pod termination) and `SIGINT` (Ctrl+C) cleanly:

1. Signal received → server stops accepting new HTTP requests and starts the hard-deadline clock (`IRONCREW_SHUTDOWN_TIMEOUT_SECS`).
2. Readiness is disabled immediately, so `/health/ready` returns `503` while teardown proceeds.
3. Active run work is aborted. Each run monitor persists an `aborted` terminal state and emits its terminal event; shutdown waits for that acknowledgement before dropping the run handle/EventBus.
4. All entries in `active_conversations` are dropped. Dropping a chat session closes its per-session `EventBus`, so SSE subscribers on `/conversations/{id}/events` unblock and their streams terminate.
5. Axum's `with_graceful_shutdown` lets remaining in-flight non-SSE requests finish.
6. Per-request `LuaCrew` instances drop → MCP managers call `shutdown_blocking()` which spawns async cleanup tasks for each stdio child / HTTP connection.
7. If Axum graceful shutdown takes longer than `IRONCREW_SHUTDOWN_TIMEOUT_SECS`, the server exits anyway (logs a warning).
8. A post-serve drain window (`IRONCREW_SHUTDOWN_DRAIN_MS`) gives the Drop-spawned MCP cleanup tasks a moment before the Tokio runtime tears down.

### Shutdown tunables

| Variable | Default | Description |
|---|---|---|
| `IRONCREW_SHUTDOWN_TIMEOUT_SECS` | `10` | Hard deadline, in seconds, counted from the moment SIGTERM/SIGINT arrives. If Axum graceful shutdown exceeds it, the process exits anyway. Keep it below the platform's configured termination grace. |
| `IRONCREW_SHUTDOWN_DRAIN_MS` | `1000` | Milliseconds to wait after Axum returns, so Drop-spawned shutdown tasks can complete. Set to `0` to skip (children will be killed when the runtime drops). |

**Tune these values** to fit your platform's grace period:

- **Kubernetes `terminationGracePeriodSeconds: 30`** (default) → leave `IRONCREW_SHUTDOWN_DRAIN_MS=1000`. Plenty of headroom.
- **Tight grace periods (≤ 10 s)** → `IRONCREW_SHUTDOWN_DRAIN_MS=500`.
- **Heavy MCP stdio usage** (many long-lived child processes per request) → bump to `2000–3000` to ensure every `uvx` / `npx` child exits cleanly.
- **Railway** → the platform default draining time is zero. The checked-in `railway.json` explicitly grants 30 seconds, so use a shutdown timeout below that value.

### Pod termination sequence (Kubernetes)

```
    kubelet              ironcrew pod
       │                      │
       │─── SIGTERM ─────────►│
       │                      │── stop accepting new requests
       │                      │── start hard-deadline clock
       │                      │       (IRONCREW_SHUTDOWN_TIMEOUT_SECS)
       │                      │── fail readiness (/health/ready → 503)
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

Ensure `terminationGracePeriodSeconds ≥ IRONCREW_SHUTDOWN_TIMEOUT_SECS + IRONCREW_SHUTDOWN_DRAIN_MS/1000 + 5s margin`. Because long-running runs are aborted on SIGTERM, you do **not** need to add `IRONCREW_MAX_RUN_LIFETIME` into this budget.

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
| `IRONCREW_MCP_ALLOWED_HTTP_HOSTS` | `__disabled__` or exact hosts | HTTP MCP frames are not bounded before rmcp decodes them. Keep disabled in production unless every exact host is operator-trusted. Public binds require this policy. |
| `IRONCREW_MCP_ALLOW_LOCALHOST` | **unset** | Only enable if MCP servers run as sidecars. |
| `IRONCREW_MAX_BODY_SIZE` | `10485760` (10 MB) or lower | Caps request body size against memory-exhaustion DoS. |
| `IRONCREW_HTTP_MAX_RESPONSE_BYTES` | `8388608` (8 MiB) or lower | Caps `http_request` and Lua `http.*` bodies. `IRONCREW_MAX_RESPONSE_SIZE` is only a deprecated fallback. |
| `IRONCREW_HITL_ENCRYPTION_KEYS` | secret JSON keyring, identical on every replica | Enables encrypted PostgreSQL cross-replica HITL for idempotency-keyed runs. Store only in Railway/OpenShift secrets; never bake it into the image. |
| `IRONCREW_HITL_ACTIVE_KEY_ID` | one id from the HITL keyring | Selects the key for new ciphertext. Both HITL variables must be set together. |
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
| `IRONCREW_READINESS_CACHE_MS` | `1000` | Coalesces public storage-aware readiness checks to protect the DB pool. |
| `IRONCREW_CONVERSATION_MAX_HISTORY` | `50` | Trim conversation history at this many non-system messages (hard ceiling 4096; zero is rejected). |
| `IRONCREW_DIALOG_MAX_HISTORY` | `100` | Trim dialog transcript at this many turns (hard ceiling 4095). |
| `IRONCREW_DIALOG_MAX_TURNS` | `1000` | Maximum accepted total turns in one dialog (hard ceiling 10000). |
| `IRONCREW_DIALOG_MAX_PARTICIPANTS` | `16` | Maximum accepted participants in one dialog (hard ceiling 64). |
| `IRONCREW_MAX_ACTIVE_CONVERSATIONS` | `8` | Max simultaneous live HTTP chat sessions in this process. Exceeding returns 503. |
| `IRONCREW_MAX_CONVERSATION_LIFECYCLES` | `256` | Bounds distinct conversation IDs with an in-flight lifecycle operation, preventing unbounded coordination-map growth (hard ceiling 4096). |
| `IRONCREW_MAX_ACTIVE_RUNS` | `4` | Max simultaneous in-flight flow runs (`POST /flows/{flow}/run`). Exceeding returns 503. |
| `IRONCREW_REQUIRE_IDEMPOTENCY_KEY` | `false` | Set `true` in production so run/message retries cannot silently duplicate work. |
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
also supports cross-replica run SSE replay. Live Lua execution, conversation
handles/SSE, and JSON/SQLite run SSE remain owner-local, so keep `replicas: 1`
when clients require those surfaces through arbitrary Service routing.
JSON/SQLite require a writable persistent volume if their records must survive
replacement.

**Railway:** the built-in PostgreSQL add-on is the simplest path. Add it and Railway sets `DATABASE_URL` automatically.

### Live-control boundary

The safest general-purpose topology remains one `serve` replica because a
second process cannot use another process's live conversation handle or take
over a dead owner's Lua VM. PostgreSQL does support a bounded multi-replica
slice: any replica can accept new keyed runs and durable reads, stream retained
run journal events, replay keyed acceptance, request keyed cancellation,
and—when the shared HITL keyring is configured—list or answer that keyed run's
pending questions. Routing affinity is not a correctness mechanism for the
remaining owner-local surfaces. Use one-shot `ironcrew run` workers only when
their workload and store ownership are intentionally separated from the HTTP
service.

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
| `IRONCREW_INSTANCE_ID` | generated per process | Optional 1–255 byte printable ASCII runtime identity written to run ownership records. Use the pod UID on OpenShift and Railway's replica ID on Railway. |
| `IRONCREW_RUN_LEASE_TTL_SECONDS` | `60` | Ownership lease expiry before unfinished-run reconciliation, and the grace before explicit indeterminate-turn recovery. Range: 1–86400. |
| `IRONCREW_HITL_ENCRYPTION_KEYS` | unset | Secret JSON object of at most 8 key ids and canonical base64 32-byte keys; maximum 16 KiB. Enables the encrypted mailbox only when the active id is also set. |
| `IRONCREW_HITL_ACTIVE_KEY_ID` | unset | Key id used for new HITL ciphertext. Configure identically on every replica. |
| `IRONCREW_ASK_HUMAN_MAX_PENDING_BYTES` | `1048576` (1 MiB) | Aggregate serialized pending-question metadata per run; range 1 byte–16 MiB. PostgreSQL ciphertext admission also accounts for 28 AEAD bytes per allowed pending row. |
| `IRONCREW_HITL_POLL_INTERVAL_MS` | `500` | PostgreSQL poll interval per pending durable question. Effective range 50–5000 ms; tune against latency and aggregate database reads. |
| `IRONCREW_HITL_READ_TIMEOUT_MS` | `2000` | Timeout for one owner-side PostgreSQL answer read. Effective range 100–30000 ms. |
| `IRONCREW_HITL_PG_MAX_CONCURRENT_READS` | `8` | Process-wide concurrency cap for PostgreSQL question-list/decrypt reads; range 1–64. |
| `IRONCREW_EVENT_JOURNAL_RETENTION_SECS` | `3600` | Logical PostgreSQL event retention; range 60–2592000 seconds. |
| `IRONCREW_EVENT_JOURNAL_MAX_TOTAL_EVENTS` | `100000` | Global logical event-count cap; range 1–10000000 and at least `IRONCREW_MAX_EVENTS`. |
| `IRONCREW_EVENT_JOURNAL_MAX_TOTAL_BYTES` | `268435456` (256 MiB) | Global logical event-byte cap; range 1 KiB–8 GiB and at least the per-run replay-byte budget. |
| `IRONCREW_EVENT_JOURNAL_PAGE_MAX_BYTES` | `524288` (512 KiB), or the event maximum when larger | Maximum bytes read in one journal page; range 1 KiB–64 MiB and at least `IRONCREW_EVENT_MAX_BYTES`. A page contains at most 64 events. |
| `IRONCREW_EVENT_JOURNAL_POLL_INTERVAL_MS` | `500` | Active-stream database poll interval; range 100–5000 ms. |
| `IRONCREW_EVENT_JOURNAL_READ_TIMEOUT_MS` | `2000` | Timeout for one journal page read; range 100–30000 ms. |
| `IRONCREW_EVENT_JOURNAL_PRUNE_BATCH` | `1000` | Maximum rows pruned per bounded pass; range 1–10000 and no greater than the global event cap. |
| `IRONCREW_ADMISSION_OBSERVATION_RATE_PER_MINUTE` | `600` | Per-principal/process rate for question-list observation; range 1–60000. |
| `IRONCREW_ADMISSION_OBSERVATION_BURST` | `20` | Per-principal/process observation burst; range 1–1000. |

IronCrew supports PostgreSQL 15+ only. This matches the session-storage
features used by the runtime and the intended deployment target of
extension-capable Postgres installs such as `pgvector`.

### HITL key rotation on Railway and OpenShift

Treat `IRONCREW_HITL_ENCRYPTION_KEYS` as encryption-key material, not ordinary
configuration. Put it in a Railway secret variable or an
OpenShift/Kubernetes `Secret` supplied by an external secret manager. The JSON
values must be canonical base64 encodings of exactly 32 random bytes.

Rotate without making in-flight questions unreadable:

1. Add the new key id/material to the secret on every replica, but leave the
   old active id selected.
2. Wait until every replica is running the expanded keyring.
3. Change `IRONCREW_HITL_ACTIVE_KEY_ID` to the new id everywhere; retain the
   old key so old ciphertext can still be decrypted.
4. After the old deployment is drained, verify that no `{prefix}human_inputs`
   row references the old question or answer key fingerprint. Answer
   consumption, timeout, terminalization, and abandoned-run reconciliation
   normally remove those rows; only then remove the old key.

Railway rolling deployments and OpenShift rolling updates can temporarily run
both revisions. A one-step key replacement is therefore unsafe: an old pod
cannot read new-key ciphertext, while a new pod cannot read old-key ciphertext.
The keyring supports at most eight keys, which leaves room for staged rotation
without unbounded secret or process memory.

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
| `GET /health/ready` | Process is ready to accept traffic and the configured persistence store responds. Returns `503` when storage is unavailable. | Startup/readiness probes and Railway deployment healthcheck. |
| `GET /health` | Backwards-compatible lightweight liveness response. | Existing monitors only; do not use as a storage-aware readiness gate. |

Provider APIs are intentionally excluded from readiness: a provider outage
should fail requests cleanly, not continuously restart or withdraw the whole
service.

### Metrics

`GET /metrics` exposes Prometheus text and is protected by the same bearer
authentication as the API. It reports process-wide active/limit gauges,
principal admission outcomes, tracked bucket count, idempotency quota
rejections, and a store-backed durable-ledger snapshot. Store reads are
coalesced for one second. The durable series expose global record/response-byte
usage and limits, global in-flight count, maximum usage by any one principal,
per-principal limits, principal count, and counts at 80/90/100 percent
saturation. Labels are fixed; principal names, tokens, keys, flow names, and
other caller-controlled values are deliberately omitted.

If the store snapshot fails, `/metrics` returns `503` rather than serving
fabricated or stale durable utilization.

Scrape with a dedicated bearer principal used only by the monitoring client,
and alert before a pod reaches its active run/conversation/SSE limits, on
sustained admission `limited` outcomes, and on idempotency utilization
thresholds. Structured tracing output remains available for Loki/Promtail or
similar log pipelines. Do not expose `/metrics` through an unauthenticated
ServiceMonitor or public exception.

The OpenShift baseline admits only ingress-controller traffic. Scrape through
the authenticated Route, or add a separate ingress rule limited to the actual
monitoring namespace/pod labels and TCP 8080; do not broaden the application
policy to every cluster namespace.

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

Do not configure session affinity as a substitute for this restriction. Affinity
can improve routing but cannot transfer run execution or conversation handles
between processes. Configured PostgreSQL run SSE and a keyed HITL mailbox work
without affinity, but neither can transfer the Lua VM or suspended coroutine;
conversation SSE and JSON/SQLite run SSE remain process-local.

---

## OpenShift specifics

The checked-in [`deploy/openshift.yaml`](../deploy/openshift.yaml) is the
production baseline. It deliberately uses:

- one replica with Deployment strategy `Recreate` as the conservative baseline
  for applications that also use owner-local conversations or unkeyed controls
- `/health/ready` for startup and readiness, and `/health/live` for liveness
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

Apply it only after replacing the image tag, example CORS origin, ConfigMap,
and Secret names:

```bash
oc apply -f deploy/openshift.yaml
```

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

- UDP/TCP 53 in `openshift-dns` (plus `kube-system` for CoreDNS-compatible
  clusters);
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
- no additional old/new deployment overlap
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
IRONCREW_EVENT_JOURNAL_PRUNE_BATCH=500
IRONCREW_MAX_SSE_CONNECTIONS=16
IRONCREW_ASK_HUMAN_MAX_PENDING=8
IRONCREW_ASK_HUMAN_MAX_PENDING_BYTES=524288
IRONCREW_HITL_POLL_INTERVAL_MS=1000
IRONCREW_HITL_READ_TIMEOUT_MS=2000
IRONCREW_HITL_PG_MAX_CONCURRENT_READS=2
IRONCREW_DB_POOL_SIZE=2
IRONCREW_SHUTDOWN_TIMEOUT_SECS=25
IRONCREW_SHUTDOWN_DRAIN_MS=1000
IRONCREW_INSTANCE_ID=${{RAILWAY_REPLICA_ID}}
IRONCREW_STORE=postgres
IRONCREW_FILE_WRITE_ROOT=/data/outputs
IRONCREW_MCP_ALLOWED_COMMANDS=__disabled__
IRONCREW_MCP_ALLOWED_HTTP_HOSTS=__disabled__
OPENAI_API_KEY=sk-...
```

Railway's Postgres add-on auto-injects `DATABASE_URL`.

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
conversations or execution horizontally safe. Scale only applications whose
routed surfaces fit the documented PostgreSQL run-SSE/keyed-control contract,
and include per-replica pools, admission, Lua memory, journal queues/pages, and
HITL polling in the aggregate budget.

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
monitoring**; use an external monitor for ongoing availability.

### 4. SIGTERM grace

Railway's default draining time is zero seconds. `railway.json` explicitly sets
30 seconds between SIGTERM and SIGKILL, so keep IronCrew's own deadline inside
that window:

```
IRONCREW_SHUTDOWN_TIMEOUT_SECS=25
IRONCREW_SHUTDOWN_DRAIN_MS=1000
```

`overlapSeconds: 0` prevents extra overlap after the candidate becomes active;
it does not make owner-local live controls distributed. Multiple replicas can
use PostgreSQL run SSE and the keyed cancellation/HITL slice, but keep the
service at one when clients require arbitrary-replica conversations or unkeyed
live controls.

### 5. Volumes

For the HTTP service, use Railway PostgreSQL instead of JSON/SQLite. If a flow
uses file-backed crew memory, mount and back up a volume for that specific
writable path; run/session PostgreSQL storage does not persist the separate
memory file.

---

## Building container images

### Source Dockerfile

The root [`Dockerfile`](../Dockerfile) uses the exact Rust `1.96.0` builder that
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
with GNU/Linux artifacts built by the release workflow using Rust `1.96.0` and
`--locked`. It has the same Debian, permissions, user, environment, and command
contract as the source image.

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
- `terminationGracePeriodSeconds` was too short. Raise it so IronCrew's drain window (`IRONCREW_SHUTDOWN_DRAIN_MS`) fits comfortably.

### `/health/live` passes but readiness is 503
- The process is alive but its configured store is unavailable. Inspect PostgreSQL connectivity and the readiness response/logs. Provider APIs are not part of readiness.

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
- [ ] `IRONCREW_MAX_RUN_LIFETIME` tuned to workload (active runs are aborted on SIGTERM, so this limit is independent of termination grace)
- [ ] `IRONCREW_REQUIRE_IDEMPOTENCY_KEY=true`; clients preserve keys across retries, and ledger byte/record caps fit the pod/database budget
- [ ] Per-principal idempotency and admission limits are set, and `429`
      saturation alerts are configured from protected `/metrics`
- [ ] `IRONCREW_SHUTDOWN_TIMEOUT_SECS + IRONCREW_SHUTDOWN_DRAIN_MS/1000 + 5s` fits within platform termination grace
- [ ] Replica count matches the
      [multi-replica live-control contract](multi-replica.md); use exactly one
      when clients require arbitrary-routed conversations or unkeyed controls;
      PostgreSQL run SSE has its own bounded shared contract
- [ ] Replacement strategy does not intentionally overlap active executors (`Recreate` on OpenShift; Railway overlap set to zero)
- [ ] Railway config-as-code and dashboard replica limits agree at the intended
      baseline (the checked-in start point is 1 vCPU / 1 GiB)
- [ ] PostgreSQL configured for production durability
- [ ] PostgreSQL event-journal logical limits, read/page/poll/prune bounds, and
      actual database/WAL/autovacuum footprint are monitored independently
- [ ] If cross-replica HITL is enabled, every replica receives the same staged
      `IRONCREW_HITL_ENCRYPTION_KEYS` secret and active key id; rotation keeps
      old keys until no mailbox row references their fingerprints
- [ ] Secrets mounted from `Secret` / vault, not baked into image
- [ ] Startup/readiness probes hit `/health/ready`; liveness hits `/health/live`
- [ ] Resource `requests` and `limits` set on the container
- [ ] OpenShift Route, DNS, PostgreSQL, and provider HTTPS verified with the
      NetworkPolicy applied; private/link-local negative probes fail
- [ ] Production image reference replaced with the signed release digest (`image@sha256:...`), not a mutable tag
- [ ] Container runs non-root with no privilege escalation and dropped capabilities
- [ ] TLS terminated at ingress / router / load balancer
- [ ] Log level set to `info` or lower (never `debug` in prod)
