# Multi-Replica Deployment Contract

This page defines what IronCrew shares between HTTP replicas, what remains
owned by one process, and what must be true before a multi-replica deployment
is considered production-safe. It is the source of truth for horizontal
deployment semantics; [HTTP Scaling](http-scaling.md) remains the capacity and
RAM-sizing guide.

The short version: **PostgreSQL is a shared durable coordination layer, not a
distributed execution engine.** The supported production topology remains one
`ironcrew serve` replica whenever clients use live run control,
Human-in-the-Loop (HITL), SSE, or HTTP conversations.

## Implementation status

Status labels on this page are deliberate:

- **Committed** means the behavior exists in the current committed code. Check
  release notes before assuming a published binary contains it.
- **Development** means it exists in the current development worktree but is
  not released yet.
- **In progress** means implementation or validation is incomplete and the
  behavior must not be used as a production contract.
- **Not implemented** means no correctness guarantee exists yet.

| Capability | Status | Production meaning |
|---|---|---|
| Shared PostgreSQL run, conversation, dialog, and audit records | Committed | Any replica using the same schema can read durable records. |
| Shared idempotency ledger, accounting, advisory locks, owner ids, and run leases | Committed | Keyed retries and database transitions coordinate across replicas. |
| Owner-aware run responses and control errors | Committed; not released | A wrong-owner request identifies the owning instance instead of pretending the live object is missing. This improves diagnosis; it does not route the request. |
| Durable cancellation of a PostgreSQL-backed keyed run from another replica | Committed; not released | The keyed-run ledger acts as a cancellation mailbox that the owner observes through its fenced heartbeat. The direct two-router PostgreSQL gate passes; separate-process, terminal-race, and owner-death gates remain before production use. |
| Cross-replica HITL answers, live SSE replay, live conversation turns, and global admission | Not implemented | These operations still require the process that owns the live object. |
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
                    records + ledgers + leases
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
| Pending HITL questions and answer bridge | Process-local | Polling or answering through another replica cannot reach the suspended coroutine. |
| Run and conversation SSE buses, replay buffers, and subscribers | Process-local | A reconnect to another replica cannot recover the live event history. Durable run history is not an SSE event journal. |
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
- it does not move pending HITL state, replay SSE events, or resume work on the
  receiving replica
- if the owner has already died, normal lease expiry and abandoned-run
  reconciliation still apply
- JSON and SQLite remain single-instance deployment backends even if their
  local cancellation behavior is unchanged

The direct two-router PostgreSQL acceptance test covers shared keyed replay,
wrong-owner HITL diagnosis, remote cancellation delivery, terminal
persistence, and terminal SSE recovery. A separate-process deployment still
must pass the full acceptance matrix below before wrong-owner cancellation is
a production contract.

## Pod death and reconciliation

There is explicitly **no execution failover or resume after pod death**.

When an owner disappears, PostgreSQL retains its run record, idempotency
claim, owner id, and lease. After lease expiry, reconciliation can mark the
run `abandoned` and preserve the original run id. It does not recreate:

- the Lua VM or task dependency graph at an execution checkpoint
- in-flight provider responses or streaming state
- tool or MCP subprocess state
- pending HITL questions and their suspended coroutine
- the run/conversation EventBus and replay buffer

A retry with the same `Idempotency-Key` preserves the original identity and
does not launch a replacement execution. Starting with a new key creates a
new run, not a continuation. External tools may have completed effects before
the pod died, so tool operations must remain independently idempotent.

## Platform contracts

### Railway

Railway [randomly distributes requests among replicas in a region and does
not support sticky sessions](https://docs.railway.com/deployments/scaling).
Consequently, standard Railway replicas cannot rely on a follow-up abort,
HITL answer, conversation message, or SSE reconnect reaching the owner.

Until all required live-control paths are shared or brokered:

- keep `numReplicas: 1` for production IronCrew HTTP services
- keep replacement overlap disabled so deployments do not create two live
  executors briefly
- use PostgreSQL and require idempotency keys, but do not mistake either for
  owner routing
- set resource and application limits per replica; Railway Pro plan headroom
  does not change the control-plane correctness boundary
- remember that database connections, resident Lua VMs, admission bursts, and
  provider concurrency multiply with the replica count

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

Keep `replicas: 1` with a non-overlapping replacement strategy for the current
production contract. For two-replica development tests, give every pod a
unique `IRONCREW_INSTANCE_ID` (the pod UID is suitable) and use per-pod test
Services or port-forwarding to target A and B deterministically. Do not count
affinity-assisted success as a passing cross-replica test.

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

- persist pending HITL question metadata without persisting secret answer
  content in logs or events
- deliver answers through an owner-consumed, fenced command channel
- add a durable bounded event journal or broker with cursor-based SSE replay
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
| 7. HITL wrong owner | Suspend a run on A in `ask_human`; list and answer through B. | B returns a truthful owner-scoped error and never reports delivery. Answering through A resumes exactly once. | Diagnostics only today; distributed HITL remains absent. |
| 8. Run SSE wrong owner | Subscribe to the active run on A, then reconnect through B. | A streams live events. B returns an owner-scoped error rather than an unrelated live stream. A terminal durable status may be returned only through an explicitly documented terminal-recovery response. | Live SSE remains process-local. |
| 9. Conversation wrong owner | Start a conversation on A; send a message and connect SSE through B; read history through B. | Message/SSE does not falsely succeed (`404` is the current boundary); durable history is readable; the turn succeeds through A exactly once. | Distributed conversation control remains absent. |
| 10. Admission scope | Saturate A's active-run and principal token-bucket limits while B is idle. | A rejects at its local limit; B retains its own local capacity; shared idempotency quotas still reject consistently across both. Metrics distinguish process and durable scopes. | Must be documented and measured before any scale-out. |
| 11. Owner death | Kill A during a keyed run without graceful shutdown; keep B alive past lease expiry. | The run becomes `abandoned`; B does not recreate the Lua VM or repeat provider/tool execution; retrying the same key preserves the original run id. | Must pass now; confirms no execution failover. |
| 12. Platform routing | Send a sequence of related requests through Railway or an OpenShift Service with affinity disabled. | Results match direct A/B tests regardless of which replica receives each request. Until phases 2–3 pass, the deployment is rejected for production stateful use. | Final multi-replica production gate. |

For release evidence, capture HTTP status/body, owner ids, run/audit rows,
idempotency rows without raw keys, provider/tool invocation counts, and the
terminal state for every case. A green unit test suite without two live
processes and real PostgreSQL is not sufficient evidence for this contract.
