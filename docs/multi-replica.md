# Multi-Replica Deployment Contract

This page defines what IronCrew shares between HTTP replicas, what remains
owned by one process, and what must be true before a multi-replica deployment
is considered production-safe. It is the source of truth for horizontal
deployment semantics; [HTTP Scaling](http-scaling.md) remains the capacity and
RAM-sizing guide.

The short version: **PostgreSQL is a shared durable coordination layer, not a
distributed execution engine.** Idempotency-keyed runs can use cross-replica
cancellation and, with an explicit shared encryption keyring, cross-replica
Human-in-the-Loop (HITL) question/answer delivery. PostgreSQL also provides a
bounded cross-replica run-event journal. Live Lua execution and HTTP
conversation handles/SSE remain process-owned. Keep one `ironcrew serve`
replica whenever clients require those or unkeyed HITL/cancellation through an
arbitrary load-balancer route.

## Implementation status

Status labels on this page are deliberate:

- **Committed** means the behavior exists in the current committed code and a
  published release contains it.
- **Committed; not released** means it exists in the current committed code,
  but no published binary contains it yet.
- **In progress** means implementation or validation is incomplete and the
  behavior must not be used as a production contract.
- **Not implemented** means no correctness guarantee exists yet.

| Capability | Status | Production meaning |
|---|---|---|
| Shared PostgreSQL run, conversation, dialog, and audit records | Committed | Any replica using the same schema can read durable records. |
| Shared idempotency ledger, accounting, advisory locks, owner ids, and run leases | Committed | Keyed retries and database transitions coordinate across replicas. |
| Owner-aware run responses and control errors | Committed; not released | A wrong-owner request identifies the owning instance instead of pretending the live object is missing. This improves diagnosis; it does not route the request. |
| Durable cancellation of a PostgreSQL-backed keyed run from another replica | Committed; not released | The keyed-run ledger acts as a cancellation mailbox that the owner observes through its fenced heartbeat. The direct two-router PostgreSQL gate passes; separate-process, terminal-race, and owner-death gates remain before production use. |
| Encrypted cross-replica HITL questions/answers for PostgreSQL-backed keyed runs | Committed; not released | With the same keyring on every replica, any replica can list a pending question and queue the first answer for owner pickup. Unkeyed/local-store runs remain process-only. |
| Bounded PostgreSQL cross-replica run SSE replay | Committed; not released | Any replica can replay/poll the shared journal using `<run_id>:<sequence>` cursors. Gaps are explicit and terminal fallback can be incomplete. JSON/SQLite remain process-local. |
| Live conversation turns/SSE and global admission | Not implemented | These operations still require the process that owns the live object or apply independently per replica. |
| Execution takeover, checkpoint resume, or provider/tool failover after owner death | Not implemented | A dead owner leaves work to be reconciled as abandoned; another replica does not resume it. |

Do not describe a multi-replica IronCrew service as highly available merely
because both pods are ready and share PostgreSQL. Today, multiple pods improve
availability for new stateless requests and durable reads, but not for the
continuation of one live execution.

## Shared state and process-local state

Two replicas have this shape:

```text
                         platform load balancer
                              /          \
                             /            \
                    replica A              replica B
                 execution + live       execution + live
                  control objects        control objects
                             \            /
                              \          /
                           PostgreSQL 15+
             records + ledgers + leases + run events
```

The database can fence ownership and preserve facts. It cannot move a Lua VM,
Tokio task, provider stream, subprocess, or in-memory event channel from one
process to another.

| State or control surface | Scope today | Consequence with two replicas |
|---|---|---|
| Run records and terminal results | Shared in PostgreSQL | History and status reads can use either replica. |
| Conversation/dialog records and transcripts | Shared in PostgreSQL | Durable history is shared, but the live Lua conversation handle is not. |
| Audit rows | Shared in PostgreSQL | Both replicas append to and query one audit history. |
| Idempotency claims, retained responses, principal quotas, and exclusive scopes | Shared in PostgreSQL | The same key and request converge on one durable result; conflicting reuse is rejected across replicas. |
| Owner instance id and run/idempotency leases | Shared in PostgreSQL | They fence stale writers and allow abandoned-run reconciliation. They do not elect a replacement worker or reconstruct execution. |
| Flow execution future, Lua VM, provider/tool calls, MCP children, and abort handle | Process-local | Only the owner can immediately stop or continue the execution. |
| Keyed-run HITL question/answer mailbox | Shared in PostgreSQL only with a shared HITL keyring | Any replica can list encrypted pending metadata or atomically queue the first encrypted answer; the owner polls and resumes its local coroutine. |
| Unkeyed or non-PostgreSQL HITL bridge | Process-local | Polling or answering through another replica cannot reach the suspended coroutine. |
| PostgreSQL run SSE journal | Shared, bounded, plaintext JSONB | Any replica can stream retained run events by cursor; writer/retention/capacity/owner-loss gaps are explicit. It is not a complete audit log. |
| JSON/SQLite run SSE and all conversation SSE | Process-local | A reconnect to another replica cannot recover the live event history. |
| Live conversation VM, turn lock, lifecycle gate, and idle timer | Process-local | A message or event request routed away from the owner cannot use its active session. The persisted transcript alone is not a live handle. |
| Active-run, active-conversation, and SSE semaphores | Process-local | Limits apply per replica. Two replicas can admit up to twice a configured per-process limit. |
| Principal token buckets and process metrics | Process-local | Admission rate and burst settings are multiplied by replica count. Durable idempotency quotas remain shared. |

All replicas must use the same flow files, authentication/principal mapping,
idempotency policy, table prefix, and relevant limits. A shared database does
not correct configuration drift between containers.

## Current owner behavior

The committed behavior remains single-executor behavior. A live-control
request that lands on another process can currently look like a missing live
object even when its durable record exists.

The committed owner-diagnostics slice changes run responses and run-control
misses as follows. It is **not released behavior yet**:

- accepted keyed runs expose `owner_instance_id` and
  `control_scope: "process"`
- a run known to be active on another process returns `409 Conflict` with
  `code: "run_owned_by_another_instance"`, the owner id, and
  `retryable: true`
- a durable record owned by the receiving process but missing its local
  control handle returns `503 Service Unavailable` with
  `code: "run_control_temporarily_unavailable"`
- a keyed PostgreSQL run with the shared HITL keyring reports
  `control_scope: "shared_store"` from its question endpoints; an accepted
  answer returns `202 queued` regardless of which replica received it
- a PostgreSQL run's event endpoint can stream the shared journal through any
  replica and reports replay ids as `<run_id>:<sequence>`
- a missing, cross-flow, or already-terminal run remains indistinguishable as
  `404` where that protects flow scoping
- a durable ownership lookup failure returns `503`, never a false success

These responses are truthful diagnostics, not a routing protocol. Clients
must not assume that an internal pod id is reachable, stable beyond the pod
lifetime, or safe to expose as a public address.

Conversation ownership is not yet represented by an equivalent distributed
control contract. A wrong-replica message or conversation SSE request can
still return `404` for a session whose durable transcript exists elsewhere.

## Durable keyed-run cancellation slice

The committed PostgreSQL slice deliberately solves only one control path:
cancelling a run that was created with an `Idempotency-Key`.

The intended sequence is:

1. Replica A accepts a keyed run and durably records its run id, attempt,
   owner, and fence.
2. A cancel request reaches replica B.
3. Replica B verifies the flow and active run record, then records a
   cancellation request in the matching in-flight PostgreSQL ledger row.
4. Replica A's fenced lease heartbeat observes the cancellation request,
   stops the worker, expires its local HITL questions, and persists the run as
   `aborted`.
5. Repeated cancellation is idempotent and cannot change an already-terminal
   result.

This does **not** make all cancellation distributed:

- it applies only to PostgreSQL-backed runs with a matching active keyed-run
  ledger; an unkeyed run still needs its owner process
- it is delivered on the owner's heartbeat interval, not as an immediate
  cross-process abort signal
- it does not move pending HITL state or resume work on the receiving replica;
  PostgreSQL run SSE is a separate shared journal, not part of cancellation
- if the owner has already died, normal lease expiry and abandoned-run
  reconciliation still apply
- JSON and SQLite remain single-instance deployment backends even if their
  local cancellation behavior is unchanged

The direct two-router PostgreSQL acceptance test covers shared keyed replay,
remote HITL delivery, remote cancellation delivery, terminal persistence, and
terminal SSE recovery. A separate-process deployment still must pass the full
acceptance matrix below before the overall multi-replica surface is a
production contract.

## Durable keyed-run HITL slice

The committed, not-yet-released PostgreSQL mailbox applies only to HTTP runs
created with an `Idempotency-Key`. Every replica must use the same
database/table prefix and the same `IRONCREW_HITL_ENCRYPTION_KEYS` keyring plus
`IRONCREW_HITL_ACTIVE_KEY_ID`.

1. The owner registers a pending question against the exact active run lease,
   idempotency digest, attempt, and owner fence.
2. Prompt/choice/timing metadata is encrypted before the row enters
   PostgreSQL. Routing and fencing identifiers remain queryable.
3. Any replica can list/decrypt the question or atomically encrypt and queue an
   answer. The first writer wins; repeats return `404`.
4. The owner polls the row, authenticates/decrypts the answer, deletes the
   mailbox entry, and resumes its process-local coroutine. The enqueue response
   is `202`, not proof that Lua has consumed the answer.
5. Timeout, cancellation, terminalization, lease loss, or run deletion fences
   further delivery and cleans up the row.

Question/answer endpoints use `Cache-Control: no-store`; answer content never
enters audit metadata or `human_input_*` SSE events. The keyring supports at
most eight canonical base64 32-byte keys and must be rotated in two stages so
all replicas can read both old and new ciphertext during a rollout. See
[Cloud Deployment](cloud-deployment.md#hitl-key-rotation-on-railway-and-openshift).

Resource use is intentionally bounded. There are at most
`IRONCREW_ASK_HUMAN_MAX_PENDING` questions per run (default 16, hard maximum
256), while `IRONCREW_ASK_HUMAN_MAX_PENDING_BYTES` bounds aggregate serialized
question metadata per run (default 1 MiB, hard maximum 16 MiB). The owner reads
each pending question every `IRONCREW_HITL_POLL_INTERVAL_MS` (default 500 ms,
effective range 50–5000) with `IRONCREW_HITL_READ_TIMEOUT_MS` (default 2000 ms,
effective range 100–30000). At defaults, a run parked on all 16 questions makes
about 32 PostgreSQL reads/second. Question-list/decrypt work is separately
bounded by `IRONCREW_HITL_PG_MAX_CONCURRENT_READS` (default 8, range 1–64).
HTTP question listing also uses the process-local observation bucket
(`IRONCREW_ADMISSION_OBSERVATION_RATE_PER_MINUTE` / `_BURST`, defaults 600/20);
internal owner polling does not. Tune all limits against aggregate replica
count, database IOPS, pool capacity, and pod RAM.

This is command delivery, not execution migration. If the owner dies, another
replica cannot recreate the suspended Lua stack or consume the answer on its
behalf. The slice does not share conversation handles. Run SSE durability is a
separate bounded plaintext journal described next.

## Bounded PostgreSQL run SSE slice

Every PostgreSQL-backed HTTP run allocates ordered journal sequences; this
does not require an idempotency key. `GET /flows/{flow}/events/{run_id}` reads
that journal on any replica and emits each retained event with
`id: <run_id>:<sequence>`. A client reconnects with the same value in
`Last-Event-ID` and receives only later sequences.

Malformed and cross-run cursors return `400`. A cursor ahead of the journal,
older than the retained boundary, or used against JSON/SQLite returns `409`.
An initial replay without a cursor emits `journal_gap` with the skipped range
and reason when necessary; its id advances through the gap's final sequence.
Successful streams use `Cache-Control: no-store, no-transform` and disable
reverse-proxy buffering.

Completeness is best-effort by design. Each producer has a bounded queue and
byte budget alongside the ordinary bounded in-memory EventBus replay, appends
bounded batches with finite retries/timeouts, and marks writer omissions as
gaps. Each reader materializes a bounded page. Retention and per-run/global
capacity also evict old events. If the run record is terminal but a numbered
`run_complete` is absent, any replica can synthesize an unnumbered completion with
`journal_complete: false`; that proves terminal state but not event-history
completeness. No replica takes over the execution that produced the journal.

Unlike the encrypted HITL mailbox, journal payloads are plaintext JSONB.
Durable `human_input_requested` records omit prompt/choices and point to the
authenticated questions endpoint; other task/model/tool/log content can be
sensitive. Every API token can read every flow's protected run events and is
therefore administrator-equivalent; principals provide accounting, not
per-flow authorization.

Per-run and global journal byte caps are logical accounting (at least 1 KiB
per event), not physical PostgreSQL quotas. They exclude indexes, tuple/page
overhead, WAL, dead tuples, replicas, and backups. Page size, poll interval,
read timeout, prune batch, SSE connection count, pool size, and actual database
growth all need separate Railway/OpenShift monitoring.

## Pod death and reconciliation

There is explicitly **no execution failover or resume after pod death**.

When an owner disappears, PostgreSQL retains its run record, idempotency
claim, owner id, and lease. After lease expiry, reconciliation can mark the
run `abandoned` and preserve the original run id. It does not recreate:

- the Lua VM or task dependency graph at an execution checkpoint
- in-flight provider responses or streaming state
- tool or MCP subprocess state
- the suspended HITL coroutine (an encrypted mailbox row may remain only until
  its expiry/cleanup fence)
- the process-local EventBus or conversation replay buffer (PostgreSQL may
  retain an explicitly incomplete run-event prefix)

A retry with the same `Idempotency-Key` preserves the original identity and
does not launch a replacement execution. Starting with a new key creates a
new run, not a continuation. External tools may have completed effects before
the pod died, so tool operations must remain independently idempotent.

## Platform contracts

### Railway

Railway [randomly distributes requests among replicas in a region and does
not support sticky sessions](https://docs.railway.com/deployments/scaling).
Consequently, standard Railway replicas cannot rely on a follow-up unkeyed
abort/HITL request or conversation message/SSE reconnect reaching the owner.
Configured keyed PostgreSQL cancellation/HITL and PostgreSQL run SSE replay do
not require owner routing.

Until all live-control paths required by an application are shared or brokered:

- keep `numReplicas: 1` when production clients require arbitrary-routed
  conversations or unkeyed live controls; multiple replicas are only within
  contract for PostgreSQL run SSE and the explicitly shared keyed-run surfaces
- keep replacement overlap disabled so deployments do not create two live
  executors briefly
- use PostgreSQL, require idempotency keys, and configure the identical HITL
  keyring on every replica that must list/answer questions
- set resource and application limits per replica; Railway Pro plan headroom
  does not change the control-plane correctness boundary
- remember that database connections, resident Lua VMs, EventBus replay,
  durable queues/pages, admission bursts, and provider concurrency multiply
  with the replica count

An application-level control broker can eventually make Railway replicas
safe. A design that depends only on load-balancer affinity cannot, because the
platform does not provide it.

### OpenShift and Kubernetes

OpenShift can optionally use Service `sessionAffinity: ClientIP`, ingress
cookie affinity, or an owner-aware internal routing layer. Affinity can reduce
wrong-owner requests, but it is an optimization rather than a correctness
mechanism:

- clients behind NAT can share an affinity identity
- reconnects, retries, ingress changes, and pod replacement can choose another
  endpoint
- affinity cannot deliver a request after the owner pod dies
- an HPA, rolling update, or manual scale changes the endpoint set

Keep `replicas: 1` when the application needs the remaining owner-local
surfaces, and use a non-overlapping replacement strategy. For two-replica
tests, give every pod a unique `IRONCREW_INSTANCE_ID` (the pod UID is suitable)
and use per-pod test Services or port-forwarding to target A and B
deterministically. Do not count affinity-assisted success as a passing
cross-replica test.

## Phased roadmap

The phases are ordered to land high-value, bounded slices before attempting
execution recovery.

### Phase 0 — truthful ownership and observability

- expose instance/control scope in accepted run and capability responses
- return structured wrong-owner and missing-local-handle errors
- expose per-instance active work and shared durable quota metrics separately
- document per-replica multiplication of memory, pool, and admission limits

**Exit gate:** wrong-replica tests fail truthfully and identify scope; no
control endpoint returns success unless the requested action was delivered.

### Phase 1 — durable keyed-run cancellation

- use the PostgreSQL keyed-run ledger as a fenced cancellation mailbox
- have the current owner observe cancellation through its run heartbeat
- make repeated requests and terminal races deterministic
- retain local immediate cancellation for owner-routed and unkeyed runs

**Exit gate:** matrix cases 4–6 pass under real PostgreSQL with two processes,
including owner death and terminalization races.

### Phase 2 — broker every live run-control path

- **Committed; not released:** persist encrypted pending HITL metadata and deliver
  encrypted answers through an owner-consumed, fenced PostgreSQL mailbox
- **Committed; not released:** persist a bounded plaintext run-event journal with
  cursor-based arbitrary-replica SSE replay and explicit gaps
- define command expiry, owner-loss, duplicate delivery, and backpressure
  semantics

**Exit gate:** abort, questions, answers, and run SSE can enter through either
replica without affinity and without false success.

### Phase 3 — conversation ownership and rehydration

- fence one active conversation turn across replicas
- rehydrate a live conversation deterministically from its persisted
  transcript and flow version, or broker the turn to its current owner
- make conversation SSE replay durable and bounded
- define behavior when flow code or model configuration changed since the
  transcript was written

**Exit gate:** start, message, history, delete, and SSE reconnect pass through
either replica while preserving exactly one active turn.

### Phase 4 — cluster-wide admission and autoscaling

- add shared or gateway-level work/control admission where global limits are
  required
- aggregate per-instance metrics without high-cardinality owner labels
- size the total PostgreSQL pool, Lua memory, provider concurrency, and SSE
  buffers as `replicas × per-replica limits`
- validate graceful scale-up, drain, scale-down, and rolling replacement

**Exit gate:** the two-replica soak stays within aggregate RAM/database/provider
budgets and scale-down never accepts control for a draining owner.

### Phase 5 — optional execution recovery

Execution resume is a separate, substantially larger project. It requires
versioned checkpoints, task/tool side-effect policy, fencing of every resumed
step, and explicit provider/tool replay semantics. Multi-replica request
routing must not be marketed as execution failover before this phase exists.

## Two-replica acceptance matrix

Run this matrix with two separate `ironcrew serve` processes, `replica-a` and
`replica-b`, using identical flows and authentication policy, unique
`IRONCREW_INSTANCE_ID` values, `IRONCREW_STORE=postgres`, the same PostgreSQL
15+ schema, and `IRONCREW_REQUIRE_IDEMPOTENCY_KEY=true`. Use direct per-replica
addresses first; repeat the applicable cases through the real platform load
balancer afterward.

Use a long-running flow, an `ask_human` flow, and an HTTP conversation flow.
Record provider/tool invocation counts outside IronCrew so duplicate effects
are visible.

| Case | Action | Required observation | Current gate |
|---|---|---|---|
| 1. Shared readiness | Start A and B against one schema; call `/health/ready` on each. | Both become ready without schema races; each reports a distinct instance id through the development capability surface. | PostgreSQL sharing committed; capability metadata development. |
| 2. Keyed run replay | Start a keyed run on A; retry the identical request and key on B. | Both responses contain the same `run_id`; the replay is marked; exactly one provider/tool execution starts. | Must pass now. |
| 3. Key conflict | Reuse that key on B with a different body or flow. | `409`; no second run or external invocation is created. | Must pass now. |
| 4. Wrong-owner diagnosis | Start a long unkeyed run on A; abort it through B. | B returns structured `409 run_owned_by_another_instance`, names A, and does not claim success. A continues until cancelled locally or completed. | Committed two-router contract test; separate-process release gate remains. |
| 5. Durable keyed cancellation | Start a long keyed run on A; cancel it through B. | B returns `cancellation_requested`; A observes the request, stops work, expires pending questions, and persists one `aborted` terminal result. | Committed real-PostgreSQL two-router gate; separate-process release gate remains. |
| 6. Cancellation races | Repeat case 5 concurrently from A and B, then repeat after terminalization. | No duplicate terminal transition; repeated requests are deterministic; a completed run is never rewritten as aborted. | In-progress PostgreSQL cancellation gate. |
| 7. HITL cross-replica delivery | Start an idempotency-keyed run on A with the shared keyring and suspend in `ask_human`; list and answer through B, then repeat the answer. | B lists the same question with `shared_store`, accepts the first answer as `202 queued`, and returns `404` for the repeat. A resumes exactly once. Wrong-flow lookup remains `404`; no plaintext answer appears in SQL, logs, audit, or SSE. | Committed/not-released real-PostgreSQL two-router gate. |
| 8. Cross-replica run SSE | Subscribe through B to a run executing on A; persist one event id, disconnect, and reconnect through either replica with that `Last-Event-ID`. Exercise malformed, cross-run, ahead, and expired cursors. | Retained events resume after the cursor without duplicates; failures return the documented `400`/`409`; gaps are explicit; terminal fallback is marked incomplete. No HITL prompt/choices appear in journal JSONB. | Committed/not-released real-PostgreSQL two-router gate. |
| 9. Conversation wrong owner | Start a conversation on A; send a message and connect SSE through B; read history through B. | Message/SSE does not falsely succeed (`404` is the current boundary); durable history is readable; the turn succeeds through A exactly once. | Distributed conversation control remains absent. |
| 10. Admission scope | Saturate A's active-run and principal token-bucket limits while B is idle. | A rejects at its local limit; B retains its own local capacity; shared idempotency quotas still reject consistently across both. Metrics distinguish process and durable scopes. | Must be documented and measured before any scale-out. |
| 11. Owner death | Kill A during a keyed run without graceful shutdown; keep B alive past lease expiry. | The run becomes `abandoned`; B does not recreate the Lua VM or repeat provider/tool execution; retrying the same key preserves the original run id. | Must pass now; confirms no execution failover. |
| 12. Platform routing | Send a sequence of related requests through Railway or an OpenShift Service with affinity disabled. | PostgreSQL run SSE and shared keyed-run operations match direct A/B tests regardless of replica. Conversation and unkeyed owner-local operations remain outside the multi-replica contract. | Per-surface production gate. |

For release evidence, capture HTTP status/body, owner ids, run/audit rows,
idempotency rows without raw keys, provider/tool invocation counts, and the
terminal state for every case. A green unit test suite without two live
processes and real PostgreSQL is not sufficient evidence for this contract.
