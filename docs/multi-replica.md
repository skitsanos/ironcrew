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
bounded cross-replica run-event journal. Live Lua execution, an active
conversation turn, and each resident handle remain process-owned. The v3.0.0
IC-008 implementation provides a narrower property: a PostgreSQL-backed keyed
message can rehydrate a cold handle from a committed durable boundary on either
replica. Its two-process PostgreSQL 15.18 acceptance and the dated OpenShift
conversation canary are complete; Railway remains unrun. The OpenShift result
used an unpublished dirty-worktree artifact, so it is platform evidence rather
than a released contract. Conversation SSE remains explicitly unsupported
with shared-store coordination. Keep one `ironcrew serve` replica whenever
clients require conversation SSE, in-flight execution takeover, or unkeyed
HITL/cancellation through an arbitrary load-balancer route. The checked-in
one-replica baseline remains the conservative default because of those
process-local boundaries; Railway replicas require their own attributed IC-008
canary.

## Implementation status

Status labels on this page are deliberate:

- **Committed** means the behavior exists in the current committed code and a
  published release contains it.
- **Available in v3.0.0+** means the behavior is part of the v3 release contract;
  publication still requires the tag-owned artifact to pass its release and
  downloaded-binary gates.
- **Implemented; not released** means the current reviewed worktree and its
  acceptance evidence contain the behavior, but it has not been committed or
  published yet.
- **In progress** means implementation or validation is incomplete and the
  behavior must not be used as a production contract.
- **Not implemented** means no correctness guarantee exists yet.

| Capability | Status | Production meaning |
|---|---|---|
| Shared PostgreSQL run, conversation, dialog, and audit records | Committed | Any replica using the same schema can read durable records. |
| Shared idempotency ledger, accounting, advisory locks, owner ids, and run leases | Committed | Keyed retries and database transitions coordinate across replicas. |
| Owner-aware run responses and control errors | Available in v3.0.0+ | A wrong-owner request identifies the owning instance instead of pretending the live object is missing. The local unkeyed separate-process diagnosis gate passes; this improves diagnosis but does not route the request. |
| Durable cancellation of a PostgreSQL-backed keyed run from another replica | Available in v3.0.0+ | The keyed-run ledger acts as a cancellation mailbox that the owner observes through its fenced heartbeat. Direct/local race gates, authoritative OpenShift v7 peer/race evidence, and retained Railway route-level delivery evidence pass. The Railway v7 rotation did not repeat cancellation. |
| Encrypted cross-replica HITL questions/answers for PostgreSQL-backed keyed runs | Available in v3.0.0+ | With the same readable key set on every replica, any replica can list a pending question and queue the first answer for owner pickup. The v3 gate adds two named agents answered through the other local PostgreSQL-backed process. OpenShift v7 delivery/rotation and Railway v7's literal bidirectional mixed cohort also pass. Unkeyed/local-store runs remain process-only. |
| Bounded PostgreSQL cross-replica run SSE replay | Available in v3.0.0+ | Any replica can replay/poll the shared journal using `<run_id>:<sequence>` cursors. The local separate-process and authoritative OpenShift v7 gates cover retained replay, malformed/cross-run/ahead/expired cursors, explicit capacity/retention gaps, read deadlines, and incomplete terminal fallback; retained Railway evidence covers numbered reconnect. JSON/SQLite remain process-local, and Railway v7 did not repeat the full cursor matrix. |
| Explicit replica drain and exact keyed-owner fencing | Available in v3.0.0+ | IC-020's local lifecycle/resource gates and temporary Railway/OpenShift drain, scale, and rolling canaries pass. Railway can continue routing to a drained process, so the application mutation fence remains mandatory. |
| Keyed PostgreSQL conversation turns and cold rehydration | Available in v3.0.0+ | `Idempotency-Key` is required, one incarnation/revision is fenced, and either of two real processes can reconstruct a cold handle. The PostgreSQL 15.18 process gate and affinity-free OpenShift IC-008 canary pass. Railway was not run. |
| Conversation SSE | Available in v3.0.0+ | PostgreSQL returns a truthful `409` and directs recovery to durable history; JSON/SQLite streams remain process-local and reject `Last-Event-ID`. Both local processes and both OpenShift replacement-cohort receivers returned the same boundary. |
| Cluster-global process admission | Not implemented | Process limits still apply independently per replica. A trusted shared gateway is required for any request/provider limit that must be global. |
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
| Conversation/dialog records and transcripts | Shared in PostgreSQL | Durable history is shared. A keyed conversation message can build a new process-local handle from the exact stored incarnation/revision; dialogs remain ordinary persisted snapshots. |
| Audit rows | Shared in PostgreSQL | Both replicas append to and query one audit history. |
| Idempotency claims, retained responses, principal quotas, and exclusive scopes | Shared in PostgreSQL | The same key and request converge on one durable result; conflicting reuse is rejected across replicas. |
| Owner instance id and run/idempotency leases | Shared in PostgreSQL | They fence stale writers and allow abandoned-run reconciliation. They do not elect a replacement worker or reconstruct execution. |
| Flow execution future, Lua VM, provider/tool calls, MCP children, and abort handle | Process-local | Only the owner can immediately stop or continue the execution. |
| Keyed-run HITL question/answer mailbox | Shared in PostgreSQL only with a shared HITL keyring | Any replica can list encrypted pending metadata or atomically queue the first encrypted answer; the owner polls and resumes its local coroutine. |
| Unkeyed or non-PostgreSQL HITL bridge | Process-local | Polling or answering through another replica cannot reach the suspended coroutine. |
| PostgreSQL run SSE journal | Shared, bounded, plaintext JSONB | Any replica can stream retained run events by cursor; writer/retention/capacity/owner-loss gaps are explicit. It is not a complete audit log. |
| JSON/SQLite run SSE and JSON/SQLite conversation SSE | Process-local | A reconnect to another replica cannot recover the live event history, and `Last-Event-ID` is rejected. |
| PostgreSQL conversation SSE | Unsupported | An existing shared-store conversation returns `409`; clients recover messages from durable history. |
| Live conversation VM, turn lock, lifecycle gate, and idle timer | Process-local cache plus shared keyed turn fence | A PostgreSQL `/messages` request does not use another process's handle. It claims the durable incarnation/revision, then reconstructs a compatible local handle when needed. No in-flight provider/tool state moves between processes. |
| Active-run, active-conversation, and SSE semaphores | Process-local | Limits apply per replica. Two replicas can admit up to twice a configured per-process limit. |
| Principal token buckets and process execution/resource metrics | Process-local | Admission rate and burst settings are multiplied by replica count. Counters and histograms reset independently per process; durable idempotency quotas and their store-backed utilization snapshot remain shared. |

All replicas must use the same flow files, authentication/principal mapping,
idempotency policy, table prefix, and relevant limits. A shared database does
not correct configuration drift between containers.

For HTTP conversations, one bounded no-follow source snapshot supplies every
executed Lua byte for the top-level config/crew, direct agent/tool files,
`_lib` modules, and nested `run_flow`; a second capture only detects a rollout
that crossed construction. The durable definition binds the selected
agent/model/prompt, provider endpoint/options, transcript/tool-round limits,
and the effective non-secret provider, Lua/JSON/HTTP/conversation-input,
reasoning, approval, dispatch, network, nested-flow, and reachable-tool-root
policies captured by the constructed runtime. A turn uses those captured
values instead of re-reading mutable environment settings. Secrets and raw
roots remain excluded, and compiled constants rely on the independently
attested artifact fingerprint. Any replica that observes source, provider,
tool-policy, or definition drift returns `409` instead of combining revisions.

## Replica identity and attested parity

Authenticated `/capabilities` separates two identities. `instance_id` is the
opaque value used in durable run ownership and `X-IronCrew-Instance-Id` response
attribution. `process_start_id` is a random UUID generated for each process
start; it changes when a platform replaces a process while reusing the same pod
or replica identity. Neither value is a routable address, and
`process_start_id` is not persisted as a run owner.

Platform acceptance may attach this optional five-field deployment tuple:

| Environment variable | Capability field |
|---|---|
| `IRONCREW_DEPLOYMENT_REVISION` | `deployment.revision` |
| `IRONCREW_ARTIFACT_FINGERPRINT` | `deployment.artifact_fingerprint` |
| `IRONCREW_FLOW_FINGERPRINT` | `deployment.flow_fingerprint` |
| `IRONCREW_CONFIG_FINGERPRINT` | `deployment.config_fingerprint` |
| `IRONCREW_HITL_KEYRING_FINGERPRINT` | `deployment.hitl_keyring_fingerprint` |

All absent produces `deployment: null`; any partial tuple fails startup. The
revision is 1–128 ASCII letters, digits, `.`, `-`, `_`, `:`, or `+`. Each
fingerprint is canonical `sha256:` plus exactly 64 lowercase hexadecimal
characters.

The tuple is an operator attestation, not a runtime measurement. Before making
a parity claim, enumerate every active process and independently hash its
actual executable, canonical flow tree, canonical resolved non-secret config,
and canonical keyring-revision manifest. Compare those observations with the
tuple returned by that exact `instance_id`/`process_start_id`; equal advertised
values or repeated load-balancer samples alone are insufficient.

The config input includes stable parity requirements such as store/schema,
auth policy shape, idempotency, pool/lease, admission/lifecycle/journal, and
runtime limits. It excludes raw or guessably hashed credentials and raw HITL
keys. The keyring input may include key ids, active id, and fingerprints of the
random 32-byte keys so matching ids backed by different material cannot pass.
Unique instance/process/platform ids, injected ports/addresses, timestamps,
and pod-specific paths are attribution rather than equality inputs. CPU/memory
limits and physical replica/surge counts are separate platform evidence.
During staged key rotation, map every process to one explicitly allowed
revision/config/keyring tuple; the intentional mixed-compatible phase is not
steady-state equality and does not excuse unrelated drift.

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

The committed, not-yet-released conversation contract is resource-based rather
than owner-routed. For PostgreSQL, every `/messages` request requires an
`Idempotency-Key`, claims the exact conversation incarnation and base revision,
and constructs a cold local handle only after that shared claim is active. The
transcript and retained replay response commit atomically. A concurrent keyed
turn or delete receives `409`; delete/recreate changes the incarnation so an
old replay cannot cross the ABA boundary. The next independent turn can land
on either replica, but an in-flight provider/tool call is never transferred.

Construction is definition-fenced. The durable identity covers the canonical
Lua source tree, selected Agent, resolved model and system prompt, transcript
limits, tool rounds, effective non-secret provider endpoint/options, and the
resolved tool graph. Provider credentials are excluded. Persistent MCP tools
require an explicit non-secret `execution_identity`. HTTP conversation
entrypoint discovery is host-enforced declarative-only, preventing network,
sub-flow, filesystem-write, run, conversation, dialog, HITL, message, and
memory-mutation effects from running before the durable turn fence.

Conversation SSE remains a separate truthful boundary: PostgreSQL returns
`409` for an existing conversation and offers durable history instead.
JSON/SQLite retain process-local SSE without cursor replay.

## Replica drain lifecycle

`ironcrew serve` uses one monotonic lifecycle per process:

```text
Accepting -> Fencing -> Draining -> Stopping
```

On Unix, `SIGUSR1` is an explicit withdraw signal. It fails readiness, fences
each exact in-flight PostgreSQL idempotency attempt still owned by this
instance, and on success leaves the process in Draining without exiting. A
failed explicit fence leaves it fail-closed in Fencing; another `SIGUSR1`
retries. Already accepted execution and observation continue. `SIGTERM` and
Ctrl+C start `IRONCREW_SHUTDOWN_ROUTING_GRACE_SECS`, retry the fence with
bounded store attempts and exponential backoff from 100 ms capped at 5 seconds
until it commits, wait any remainder of the routing interval, and only then
enter Stopping, where active work is cancelled and terminalized within the
bounded shutdown policy.
Repeated signals cannot move the lifecycle backwards.

The public drain contract is:

- `/health/ready` returns `503` with `component: "lifecycle"` and the current
  `lifecycle_state` (`fencing`, `draining`, or `stopping`) once withdrawal
  begins; liveness remains available until process exit.
- A direct protected `POST` or `DELETE` whose lifecycle middleware check occurs
  after withdrawal returns non-cacheable `503` with
  `code: "instance_draining"`, the current `lifecycle_state`, and numeric
  `Retry-After: 1`. That middleware snapshot is the mutation-admission
  linearization point. A request admitted while still Accepting can lose an
  inner race and return a generic non-cacheable `503` with numeric
  `Retry-After` instead of the structured lifecycle body.
- A peer that observes the exact durable owner/attempt fence returns
  non-cacheable `503` with `code: "run_owner_draining"` instead of claiming
  that cancellation or an HITL answer was accepted. It also carries
  `Retry-After: 1`. This durable check applies to PostgreSQL-backed
  idempotency-keyed attempts; it is not a general live object router.
- Protected observation remains available while Draining: capabilities,
  metrics, run/status reads, question reads, and run SSE do not consume a work
  or control mutation. Observation still obeys authentication, ordinary
  admission, retention, and owner-local boundaries.
- Authenticated protected responses expose `X-IronCrew-Instance-Id`, so direct
  acceptance and platform canaries can attribute the receiving process without
  treating that opaque id as a routable address. Browser clients may read the
  header through CORS.
- `/capabilities` also exposes the per-start `process_start_id`, optional
  deployment evidence, and top-level `lifecycle_state`. Protected metrics expose
  the one-hot `ironcrew_process_lifecycle_state{state="..."}` gauge for the
  four fixed state values and
  `ironcrew_process_lifecycle_rejections_total{class="work|control"}` for
  direct mutation rejections. Instance, owner, run, key, flow, and principal
  values never become metric labels.

Fencing narrows the race between platform routing and owner-directed control;
it does not guarantee that a load balancer has stopped routing, that an
arbitrarily long run finishes, that an in-flight conversation turn transfers,
or that execution recovers after `SIGKILL`.

### Admission ownership

IronCrew deliberately does not turn every limit into a database lock:

| Enforcement owner | Contract |
|---|---|
| No wider than one replica | Active-run, active-conversation, lifecycle-key and SSE semaphores; work/control/observation buckets; PostgreSQL pool size; per-crew task concurrency; provider-instance pacing inside each live Lua VM; event buffers; CPU and RAM. Aggregate declared per-replica limits as `replicas × per-replica limit`; provider work needs a separate planning envelope because it has no process-wide semaphore. |
| Shared PostgreSQL table prefix | Idempotency records, in-flight claims, retained response bytes and per-principal budgets; journal-global event/byte budgets; leases, exact attempt fences, keyed cancellation, and encrypted HITL mailbox transitions. Do not multiply these logical caps by replica count. |
| Trusted gateway | Any desired cluster-wide request rate, concurrent-request ceiling, or provider/API budget. The gateway must authenticate before policy, bound queues/waits, fail closed, and preserve idempotency keys across retries. |

The process-local token buckets and provider pacing variables are safe overload
controls, but they must never be advertised as cluster-wide quotas. PostgreSQL
coordinates durable identity and bounded shared state; it is not the right
place to hold a database transaction open around provider execution.

### Metrics aggregation ownership

Every `ironcrew_runs_total`, task, tool, provider, token, SSE, lease-loss,
reconciliation, terminal-persistence, and store-failure sample belongs to the
single process that served that scrape. The duration histograms have the same
scope. Their counters, buckets, sums, and counts reset independently whenever
that process restarts; PostgreSQL does not merge or restore them.

Scrape every pod/instance as a unique authenticated target, preserve that
identity in the monitoring system, and aggregate only after target
deduplication:

- compute fleet totals from `sum` of per-target `rate()`/`increase()` values,
  including old and new deployment pods only when the question intentionally
  covers the overlap window;
- compute a fleet histogram quantile by summing bucket rates by `le` plus the
  closed dimensions to retain, then apply `histogram_quantile`;
- evaluate maintenance health per pod or take its minimum, group the one-hot
  lifecycle gauge by `state`, and do not sum either boolean contract; and
- do not sum the store-backed `ironcrew_idempotency_*` gauges. Each replica with
  the same PostgreSQL table prefix reports the same shared snapshot, so use one
  target or `max` after verifying configuration parity.

Railway's public load balancer can repeatedly return the same replica, so a
public `/metrics` request is not fleet evidence. Use private/platform target
discovery that reaches each Railway instance. On OpenShift/Kubernetes, discover
and scrape each pod with the bearer credential and a narrowly scoped
NetworkPolicy allowance; a single Service/Route sample does not prove pod
coverage. IronCrew intentionally supplies fixed-cardinality process telemetry,
not a hosted Prometheus backend or cross-replica collector.

## Durable keyed-run cancellation slice

The committed PostgreSQL slice deliberately solves only one control path:
cancelling a run that was created with an `Idempotency-Key`.

The intended sequence is:

1. Replica A accepts a keyed run and durably records its run id, attempt,
   owner, and fence.
2. A cancel request reaches replica B.
3. Replica B verifies the flow and active run record, then records a
   cancellation request in the matching in-flight PostgreSQL ledger row and
   deletes that run's encrypted shared HITL mailbox rows in the same
   transaction.
4. Replica A's fenced lease heartbeat observes the cancellation request,
   stops the worker, expires its local HITL questions, and persists the run as
   `aborted`.
5. Repeated cancellation is idempotent and cannot change an already-terminal
   result.

If A dies before step 4 commits its terminal compare-and-set, normal lease
reconciliation records `abandoned`. The completed ledger retains the original
acceptance response and the cancellation timestamp as historical evidence,
while the active lease and HITL mailbox are cleared. Reconciliation does not
append a fictional durable `run_complete`; PostgreSQL SSE exposes the
authoritative run row through an unnumbered, explicitly incomplete fallback.

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
database/table prefix and readable `IRONCREW_HITL_ENCRYPTION_KEYS` key set.
Steady state uses the same `IRONCREW_HITL_ACTIVE_KEY_ID`; a controlled rotation
may temporarily mix active ids only after every process has both old and new
keys.

1. The owner registers a pending question against the exact active run lease,
   idempotency digest, attempt, and owner fence.
2. Prompt/choice/timing metadata is encrypted before the row enters
   PostgreSQL. Routing and fencing identifiers remain queryable.
3. Any replica can list/decrypt the question or atomically authenticate it and
   queue an answer encrypted under the question's fingerprint. The first writer
   wins; repeats return `404`.
4. The owner polls the row, authenticates/decrypts the answer, deletes the
   mailbox entry, and resumes its process-local coroutine. The enqueue response
   is `202`, not proof that Lua has consumed the answer.
5. Timeout, cancellation, terminalization, lease loss, or run deletion fences
   further delivery and cleans up the row.

Question/answer endpoints use `Cache-Control: no-store`; answer content never
enters audit metadata or `human_input_*` SSE events. The keyring supports at
most eight canonical base64 32-byte keys. Rotation uses two compatibility
stages—expanded/old-active, then expanded/new-active—followed by a separate
new-only retirement rollout after both fingerprint columns have zero old-key
references. Startup refuses retained ciphertext whose key is absent. See
[Cloud Deployment](cloud-deployment.md#hitl-key-rotation-on-railway-and-openshift).

Resource use is intentionally bounded. There are at most
`IRONCREW_ASK_HUMAN_MAX_PENDING` questions per run (default 16, hard maximum
256), while `IRONCREW_ASK_HUMAN_MAX_PENDING_BYTES` bounds aggregate serialized
question metadata per run (default 1 MiB, hard maximum 16 MiB). The owner reads
each pending question every `IRONCREW_HITL_POLL_INTERVAL_MS` (default 500 ms,
effective range 50–5000) with `IRONCREW_HITL_READ_TIMEOUT_MS` (default 2000 ms,
effective range 100–30000). At defaults, a run parked on all 16 questions makes
about 32 PostgreSQL reads/second. Question-list decryption and answer-side
question authentication share `IRONCREW_HITL_PG_MAX_CONCURRENT_READS`
(default 8, range 1–64).
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
gaps. For `W = IRONCREW_EVENT_JOURNAL_WRITE_TIMEOUT_MS` (default 1500 ms,
range 100–5000), PostgreSQL lock/statement waits are `4W/5`; a batch gets three
outer attempts separated by 50/100 ms, and flush/terminal acknowledgement is
bounded to `3W + 650 ms`. That last bound includes queue admission but is not a
promise to drain an arbitrary backlog. Each reader materializes a bounded page.
The terminal run row precedes the numbered terminal append, so terminal run
status is not a journal-flush barrier. A read can truthfully return the
unnumbered incomplete fallback while idempotency finalization and the bounded
terminal append are still pending. Clients that require a resumable terminal
cursor must retry with their own bounded policy.
Retention and per-run/global capacity also evict old events. If the run record
is terminal but a numbered `run_complete` is absent, any replica can synthesize
an unnumbered completion with `journal_complete: false`; that proves terminal
state but not event-history completeness. No replica takes over the execution
that produced the journal.

Unlike the encrypted HITL mailbox, journal payloads are plaintext JSONB.
Durable `human_input_requested` records omit prompt/choices and point to the
authenticated questions endpoint; other task/model/tool/log content can be
sensitive. Every API token can read every flow's protected run events and is
therefore administrator-equivalent; principals provide accounting, not
per-flow authorization.

Per-run and global journal byte caps are logical accounting (at least 1 KiB
per event), not physical PostgreSQL quotas. They exclude indexes, tuple/page
overhead, WAL, dead tuples, replicas, and backups. Page size, poll interval,
read/write timeouts, prune batch, SSE connection count, pool size, and actual
database growth all need separate Railway/OpenShift monitoring.

## Pod death and reconciliation

There is explicitly **no in-flight execution failover or resume after pod
death**.

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

Production accepts `IRONCREW_RUN_LEASE_TTL_SECONDS=6..86400` (default 60), and
owner heartbeats plus replica maintenance run every TTL/3. A keyed owner starts
its process-local deadline when each durable claim or renewal is invoked, never
when the response arrives, so a slow successful database round trip consumes
rather than extends the remaining side-effect window.

PostgreSQL bounds lock and statement work inside each maintenance transaction,
then applies a slightly larger outer watchdog to both the owner-heartbeat and
reconciliation calls. In steady state, a healthy peer therefore nominally
observes a dead owner within `TTL + one cadence + bounded heartbeat and
reconciliation work`—about 90 seconds at the 60-second default when the run is
in the next fixed 64-row reconciliation batch. Larger backlogs add cadence
windows; repeated database failures or scheduler starvation can extend the
window further. A bounded maintenance failure makes `/health/ready` report
`503` with `component: "storage_maintenance"`; healthy in-flight maintenance
does not create transient failures, and readiness returns only after both
operations succeed in one cycle. This fencing and detection behavior still
does not move or resume execution.

A retry with the same `Idempotency-Key` preserves the original identity and
does not launch a replacement execution. Starting with a new key creates a
new run, not a continuation. External tools may have completed effects before
the pod died, so tool operations must remain independently idempotent.

Conversation turns add a narrower recovery property. If the owner dies between
completed turns, a later keyed message can cold-rehydrate the committed
transcript on a surviving replica. If the owner dies after provider/tool work
may have started, the active key becomes indeterminate; another key cannot
silently bypass its incarnation-scoped hazard. The client must inspect durable
history and use the documented `Idempotency-Recovery-Key` barrier. This is a
new turn from a committed boundary, not continuation of the dead Lua VM.

## Platform contracts

### Railway

Railway [randomly distributes requests among replicas in a region and does
not support sticky sessions](https://docs.railway.com/deployments/scaling).
Consequently, standard Railway replicas cannot rely on a follow-up unkeyed
abort/HITL request or conversation SSE reconnect reaching the owner.
Configured keyed PostgreSQL cancellation/HITL and PostgreSQL run SSE replay do
not require owner routing. The committed IC-008 implementation gives keyed
PostgreSQL conversation messages the same routing independence only at
committed turn boundaries. Its local two-process gate passes, but Railway
routing was not run for IC-008 and remains a separate release gate.

Until all live-control paths required by an application are shared or brokered:

- keep Railway `numReplicas: 1` because IC-008 conversation routing has not
  been run there, and keep one replica on every platform whenever production
  clients require conversation SSE, in-flight conversation takeover, or
  unkeyed live controls; the OpenShift pass does not make keyed conversation
  turns a published or Railway contract
- keep replacement overlap disabled so deployments do not create two live
  executors briefly
- use PostgreSQL, require idempotency keys, and configure the identical HITL
  keyring on every replica that must list/answer questions
- leave `IRONCREW_INSTANCE_ID` unset on Railway so IronCrew validates and uses
  the runtime-only `RAILWAY_REPLICA_ID` automatically; service-variable
  interpolation produced an empty explicit value in the 2026-08-10 sandbox
  canary and must not be used. Because an in-place restart can reuse that id,
  compare `process_start_id` before and after replacement
- set resource and application limits per replica; Railway Pro plan headroom
  does not change the control-plane correctness boundary
- remember that database connections, resident Lua VMs, EventBus replay,
  durable queues/pages, admission bursts, and provider concurrency multiply
  with the replica count
- for steady `R = 2`, budget an old/new deployment that permits full overlap
  as `P = 2R = 4` simultaneous containers: with the checked-in per-replica
  slice that is 8 active runs, 16 resident conversations, 64 SSE connections,
  8 PostgreSQL pool connections, and 4 GiB of platform memory allocation
- monitor `/health/ready` continuously: Railway's configured healthcheck is a
  deployment gate, so a later maintenance `503` needs external alerting rather
  than being assumed to withdraw an active replica automatically

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

The dated IC-008 OpenShift canary passed keyed committed-boundary turns through
affinity-free Routes, including cold recovery after owner Pod deletion. That
artifact was dirty, unpublished, and removed, so the checked-in one-replica
baseline remains until a published release contains the behavior. Keep
`replicas: 1` whenever the application needs the remaining owner-local
surfaces, and use a non-overlapping replacement strategy. For two-replica
tests, give every pod a unique `IRONCREW_INSTANCE_ID` (the pod UID is suitable)
and use per-pod test
Services or Routes to target A and B deterministically. Do not count
affinity-assisted success as a passing cross-replica test. Use
`/health/ready` as the readiness probe and `/health/live` as liveness: a
lease-maintenance failure should remove the pod from Service endpoints, not
restart it while PostgreSQL remains contended.

For a bounded two-replica rollout, use `maxSurge: 1`, `maxUnavailable: 0`, and
a measured non-zero `minReadySeconds` stabilization window, but do not treat
`P = R + 1 = 3` as a physical resource ceiling: terminating pods are not
counted in the Deployment controller's surge limit. For one controlled rollout,
conservatively budget old `R` + new `R` = 4 physical pods until both old pods
exit: 8 active-run slots, 16 resident-conversation slots, 64 SSE connections,
8 PostgreSQL pool connections, and 4 GiB of platform memory allocation with
the checked-in per-replica slice. Overlapping rollouts or manual deletion can
exceed that envelope; use observed pod counts and do not overlap rollouts. A
preStop sleep consumes the same `terminationGracePeriodSeconds` budget as
IronCrew's routing drain and stop; do not add the intervals as though kubelet
waits for one budget before starting the next. Clean-exit arithmetic assumes
the durable fence commits inside routing grace; a prolonged fence failure stays
Fencing until the platform may use `SIGKILL`, then reconciles unfinished work
as Abandoned.

Readiness checks cache their storage result and coalesce overlapping uncached
probes behind one check for up to one second. That prevents ordinary
kubelet/operator probe overlap from producing a contention-only `503`; a
genuinely stalled check still fails closed. `minReadySeconds` remains required
so a single transiently healthy probe cannot make the controller retire the
last stable old endpoint.

### First drain-aware rollout

The first version that introduces the durable drain fence cannot make an old
binary honor that fence. During a mixed-version rollout, an old receiving
replica can still accept owner-directed control without checking whether the
new owner is draining, and an old owner cannot publish the new fence before it
terminates. Use a maintenance window, `Recreate`/scale-to-zero transition, or
an externally verified zero-active-work cutover for this first rollout. Do not
enable explicit drain or claim the rolling-replacement contract until every
possibly routed replica reports the drain-aware capability. Later homogeneous
rollouts may use the documented signal and fence sequence.

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

**Exit gate:** the local PostgreSQL process gate now passes matrix cases 4–6
and owner-death case 11, including terminalization and kill-during-cancellation
races. Railway/OpenShift routing remains a separate deployment gate.

### Phase 2 — broker every live run-control path

- **Available in v3.0.0+:** persist encrypted pending HITL metadata and deliver
  encrypted answers through an owner-consumed, fenced PostgreSQL mailbox
- **Available in v3.0.0+:** persist a bounded plaintext run-event journal with
  cursor-based arbitrary-replica SSE replay and explicit gaps
- define command expiry, owner-loss, duplicate delivery, and backpressure
  semantics

**Exit gate:** abort, questions, answers, and run SSE can enter through either
replica without affinity and without false success.

### Phase 3 — conversation ownership and rehydration

- **Available in v3.0.0+:** fence one active
  conversation incarnation/revision across replicas with a required
  idempotency key
- **Available in v3.0.0+:** rehydrate a cold local
  handle deterministically from its persisted transcript and complete
  definition identity
- **Available in v3.0.0+:** return a truthful `409`
  unsupported boundary for PostgreSQL conversation SSE; JSON/SQLite remain
  process-local without cursor replay
- **Available in v3.0.0+:** fail closed when Lua source,
  selected agent/model/system prompt, captured limits and policies, provider
  endpoint/options, or resolved tool graph changed

**Exit gate:** start, keyed message, durable history, delete, owner death, and
rolling restart pass through either real replica while preserving one active
turn. Both replicas return the same documented SSE unsupported response. The
2026-08-11 local two-process PostgreSQL gate satisfies this phase. An attributed
platform load-balancer canary is still required to change the conservative
deployment baseline; it is not part of the claimed local result.

### Phase 4 — cluster-wide admission and autoscaling

- retain process-local admission where the intended limit is per replica, use
  the existing PostgreSQL quotas only for durable shared limits, and require a
  trusted gateway where work/control/provider admission must be cluster-global
- expose per-instance lifecycle metrics with fixed labels
- size process-local run, conversation, PostgreSQL-pool, base top-level Lua-VM,
  and SSE budgets as `replicas × per-replica limits`; nested sub-flow VMs add
  beyond the base formula, shared ledger/journal limits do not multiply, and
  provider task-slot arithmetic is not a global provider cap
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
15+ schema, and `IRONCREW_REQUIRE_IDEMPOTENCY_KEY=true` except for the
deliberately unkeyed case 4 pair. Use direct per-replica addresses first;
repeat the applicable cases through the real platform load balancer afterward.

For a platform run, require the complete deployment-evidence tuple on each
process. Match the active platform inventory to distinct instance/process-start
pairs, verify the artifact/flow/config/keyring hashes independently inside each
process, and record planned rotation differences explicitly. Do not put unique
ids or platform limits into the steady-state config-equality hash; retain them
as separate attribution and capacity evidence.

Use a long-running flow, an `ask_human` flow, and an HTTP conversation flow.
Record provider/tool invocation counts outside IronCrew so duplicate effects
are visible.

| Case | Action | Required observation | Current gate |
|---|---|---|---|
| 1. Shared readiness | Start A and B against one schema; call `/health/ready` on each. | Both become ready without schema races; each reports a distinct instance id through the development capability surface. | Local PostgreSQL and authoritative OpenShift v7 pass. Retained Railway routing and six independently attested v7 rotation processes prove distinct ready identities. |
| 2. Keyed run replay | Start a keyed run on A; retry the identical request and key on B. | Both responses contain the same `run_id`; the replay is marked; exactly one provider/tool execution starts. | Local, authoritative OpenShift v7, and retained Railway route replay pass. OpenShift retained an exact external mock-effect counter; this is not a general exactly-once guarantee. |
| 3. Key conflict | Reuse that key on B with a different body or flow. | `409`; no second run or external invocation is created. | Local, authoritative OpenShift v7, and retained Railway route conflict evidence pass. The Railway v7 rotation did not repeat this case. |
| 4. Wrong-owner diagnosis | Start a long unkeyed run on A; abort it through B. | B returns structured `409 run_owned_by_another_instance`, names A, and does not claim success. A continues until cancelled locally or completed. | Local and authoritative OpenShift v7 return the truthful `409` boundary without routing control or duplicating the effect. This remains an unsupported owner-local surface, not a promise that every platform reruns it. |
| 5. Durable keyed cancellation | Start a long keyed run on A; cancel it through B. | B returns `cancellation_requested`; A observes the request, stops work, expires pending questions, and persists one `aborted` terminal result. | Local and authoritative OpenShift v7 peer delivery pass; retained Railway route evidence also terminalized once, although that early receiver was not attributable. |
| 6. Cancellation races | Repeat case 5 concurrently from A and B, then repeat after terminalization. | No duplicate terminal transition; repeated requests are deterministic; a completed run is never rewritten as aborted. | Local and authoritative OpenShift v7 converge on one terminal event with no mailbox row or false post-terminal success. Railway v7 did not repeat the concurrent race. |
| 7. HITL cross-replica delivery and rotation | Start an idempotency-keyed run on A with the shared keyring and suspend in `ask_human`; list and answer through B, then repeat. Roll the two processes through expanded/old-active, expanded/new-active, and new-only revisions while retaining real mailbox rows. | B lists the same question with `shared_store`, accepts the first answer as `202 queued`, and returns `404` for the repeat. A resumes exactly once. Premature key removal fails startup without row mutation; old-question answers stay on the old fingerprint; new-only starts only after zero old references. No plaintext answer appears in SQL, logs, audit, or durable SSE. | Local and OpenShift v7 rotation pass. Railway v7 passed a literal real-overlap 2-old/2-new bidirectional delivery, zero old references, and final two-process new-only peer run with exact process inventories. |
| 8. Cross-replica run SSE | Subscribe through B to a run executing on A; persist one event id, disconnect, and reconnect through either replica with that `Last-Event-ID`. Exercise malformed, cross-run, ahead, and expired cursors. | Retained events resume after the cursor without duplicates; failures return the documented `400`/`409`; gaps are explicit; terminal fallback is marked incomplete. No HITL prompt/choices appear in journal JSONB. | Local and authoritative OpenShift v7 cover numbered replay, reconnect, explicit gap, and the complete cursor-error matrix. Retained Railway evidence covers numbered reconnect; v7 rotation proved seven-event terminal barriers but did not repeat every cursor error. |
| 9. Conversation rehydration and fencing | Start a conversation on A; send a keyed cold turn through B; replay through A; compare history; restart B, kill A between turns, continue through B; race same- and peer-replica deletes against a blocked turn; delete/recreate; request SSE from both. | One provider effect per logical key; response and history retain one incarnation/definition and monotonic revision; cold/owner-death turns succeed through B; both active deletes promptly return `409`; recreate gets a new incarnation and old replay is fenced; both SSE requests return the same `409` unsupported boundary. | Local PostgreSQL 15.18 passes with two real `serve` processes: the serial combined gate passed 60/60 and the release-binary soak passed 2/2. The independently attested OpenShift canary also passes through affinity-free Routes, including a separate cold keyed recovery after owner deletion. Railway remains unrun, and the dirty artifact was unpublished and removed. |
| 10. Admission scope | Saturate A's active-run and principal token-bucket limits while B is idle. | A rejects at its local limit; B retains its own local capacity; shared idempotency quotas still reject consistently across both. Metrics distinguish process and durable scopes. | Local and authoritative OpenShift v7 pass per-pod run/conversation/SSE limits and shared durable quota. Railway v7 retained exact rollout-limit arithmetic, not another saturation run. A trusted gateway still owns any global request/provider cap. |
| 11. Owner death | Kill A during a keyed run without graceful shutdown; keep B alive past lease expiry. | The run becomes `abandoned`; B does not recreate the Lua VM or repeat provider/tool execution; retrying the same key preserves the original run id. | Local and authoritative OpenShift v7 pass actual `SIGKILL`, peer reconciliation, stable replay/effect count, and distinct replacement. Retained Railway lifecycle evidence covers bounded owner replacement; Railway did not repeat `SIGKILL`. |
| 12. Platform routing | Send a sequence of related requests through Railway or an OpenShift Service with affinity disabled. | PostgreSQL run SSE and shared keyed-run operations match direct A/B tests regardless of replica. IC-008 keyed conversation turns require their own attributed platform decision; conversation SSE and unkeyed owner-local operations remain outside the portable surface. | OpenShift v7 passes the earlier run-control matrix, and the separate IC-008 OpenShift canary passes committed-boundary conversation routing. Railway evidence remains cumulative only for earlier surfaces; no Railway IC-008 canary was run. Neither result provides conversation SSE or in-flight takeover. |
| 13. Explicit drain and replacement | Hold a keyed run on A, send `SIGUSR1`, observe A directly and through B, then send `SIGTERM`, replace A on the same direct address, and repeat with signal-driven termination from Accepting. | A becomes unready before mutation rejection; the exact owner/attempt is durably fenced; direct mutations return `instance_draining`; B returns `run_owner_draining` for owner-directed control; protected reads/SSE/metrics remain observable; exactly one terminal transition is persisted; replacement C accepts new work; every shutdown stays inside the declared timing/resource envelope. | Local, retained Railway/OpenShift IC-020 canaries, and authoritative OpenShift v7 pass. OpenShift's shared Route stayed continuous through v7 replacement; Railway's application fence remained authoritative when its router still reached a drained process. |

For release evidence, capture HTTP status/body, owner ids, run/audit rows,
idempotency rows without raw keys, provider/tool invocation counts, and the
terminal state for every case. Also retain the independently verified
revision/artifact/flow/config/keyring tuple and every observed
`process_start_id`. A green unit test suite without two live
processes and real PostgreSQL is not sufficient evidence for this contract.

The IC-020 evidence is separate from the older IC-007 feature canaries below.
The same-host gate proves the exact owner/attempt fence and durable lifecycle;
the capacity receipt proves bounded local scaling; and the dated Railway and
OpenShift canaries prove their respective routing and shutdown behavior. None
of those IC-020/IC-007 canaries is IC-008 keyed-conversation evidence,
provides in-flight execution takeover, or replaces the long IC-018
retention-boundary soak; the separate IC-008 canary is recorded below.

### Dated local evidence

On 2026-08-11, IC-008 passed its exact serial PostgreSQL 15.18 gate 60/60
(48 store, 2 multi-router, and 10 separate-process acceptance tests). Case 9
used two independent `ironcrew serve` OS processes and proved peer start,
required-key cold messages, concurrent same-key replay with one provider
effect, durable history, prompt same/peer active-delete conflicts,
delete/recreate incarnation fencing, both-process SSE `409`, restart, and real
owner `SIGKILL` between committed turns. The locked-release provider-free soak
then passed 2/2, observed two identities across 32 samples split 16/16, and
recorded zero failures, deadlocks, or forced kills. The isolated database,
role, exact table prefix, labeled `--rm` container, Python caches, and temporary
reports were removed; the `postgres:15` image was retained. No platform command
was run by that local gate. The separate OpenShift result below supplies
platform evidence, while its unpublished-artifact boundary leaves the
checked-in one-replica posture unchanged.

### Authoritative IC-008 OpenShift conversation evidence — 2026-08-11

The independently attested dirty-worktree artifact passed the applicable
conversation matrix in a temporary `restricted-v2` deployment. The
affinity-free shared Route returned 64/64 valid initial capability responses
split 32/32 across A/B and 32/32 replacement responses split 16/16 across B/C.
Start through both processes, required-key turns, exact replay without another
mock effect, durable history, replacement rehydration, same- and peer-process
active-delete `409`, delete/recreate incarnation fencing, and the shared-store
conversation-SSE `409` all passed. All protected responses retained receiver
attribution and `Cache-Control: no-store`.

The accepted owner-loss proof force-deleted a Pod only between committed
turns. A separate cold case showed C had no live handle or prior operation for
the conversation before it committed the next keyed turn directly from
PostgreSQL with stable incarnation/source/definition and exactly one counted
effect. This is not in-flight Lua/provider/tool takeover and does not establish
general exactly-once effects. PostgreSQL conversation SSE remains unsupported;
durable history is the recovery surface. Railway was not run for IC-008.

The exact [human receipt](../evaluations/platform-canary/reports/ic008-openshift.md)
and [machine receipt](../evaluations/platform-canary/reports/ic008-openshift.json)
hash to
`sha256:acff73fd9e7f6233a45c00791892813941ad4441e2d8e2810a3133502d098dcb`
and
`sha256:069848c1d1cc598743d9207350079b3a78919916c11e260202339b737315a6e4`.
They bind dirty source manifest `sha256:fdf9b1813eaf914f03089931c1016134983c9c1c7ce622e85bbd32ae6eb2414e`,
Linux/amd64 binary `sha256:1cb77b4f712381a8aa2226fd1963f576bccb54d350329cf88e420806d4e0c4f3`,
and OCI manifest
`sha256:ad413aff04e3eae80c5c3b82e3b03e9387c5a15809e24a94992804f14e4ac29a`.
The artifact was unpublished and removed; those digests are observed identity,
not a downloadable or bit-reproducible release.

Cleanup returned the exact database prefix/functions, labeled objects, quota,
local staging/cache, and attributable Docker resources to zero, and restored
the namespace baseline at
`sha256:ce9697dfb8eb519641338240dcbb0ab328952ebc8b07c9500a511101d774d4dd`.
The namespace's additive same-namespace NetworkPolicy prevents an exclusive
isolation claim. Initial A/B final log tails and the inline orchestration
wrapper bytes were not retained. Docker Scout reported four unfixed
Debian-base findings—two CRITICAL and two HIGH—so no security-clean claim is
made.

On 2026-08-10, the IC-020 target passed 2/2 in 6.21 seconds against PostgreSQL
15 with two independent `ironcrew serve` processes.
The provider-free fixture covered explicit `SIGUSR1` drain, signal-driven
termination from Accepting, a real blocked PostgreSQL fence with fail-closed
retry, direct and peer rejection bodies/headers, readiness, capabilities,
metrics, retained question reads, post-fence question-registration refusal,
durable mailbox/ledger/event invariants, terminalization, same-key replay, and
replacement-process acceptance. The disposable schema and named `--rm`
container were removed after the gate. This proves the same-host process and
PostgreSQL lifecycle under that fixture.

The release-binary capacity gate then scaled `R=1/2/3` processes against
PostgreSQL 15 while holding two loopback-provider calls and two SSE streams per
process. PostgreSQL connections and provider peaks were `2/4/6`; aggregate
host RSS was approximately 16.9/34.3/51.8 MiB against 256 MiB per process; and
all active/EventBus gauges returned to zero within 957 ms after each phase.
See the reviewed
[summary](../evaluations/replica-lifecycle/reports/2026-08-10-local-postgres15.md)
and machine report beside it.

The temporary Railway lifecycle canary scaled `1 -> 2 -> 1` with exact runtime
replica attribution. Direct drain and peer-control rejections matched the
documented bodies; `SIGTERM` persisted one `Aborted` terminal event after the
five-second routing grace; same-key replay stayed on the original run; and an
in-place restarted process completed a second peer-answered run. Railway kept
routing some reads to the drained process and reused its replica ID after
restart, so readiness withdrawal and replacement identity must not be inferred.

The OpenShift `restricted-v2` canary proved `1 -> 2 -> 1`, EndpointSlice
withdrawal, peer drain fencing, clean PID 1 `SIGTERM`, one `Aborted` terminal
event, and stable replay. Its first rolling run exposed a real
contention-only readiness `503`; bounded readiness singleflight and a
10-second `minReadySeconds` window corrected it. In the homogeneous rerun, both
direct replicas passed 64/64 parallel readiness, the stable peer stayed ready,
and the affinity-free shared Route passed 60/60 readiness, 60/60 liveness, and
60/60 capability probes across the rollout. The retiring direct route returned
one lifecycle `503` and one timeout while its old endpoint disappeared; the
shared Route remained continuous.

On 2026-07-19, commit `668f313` passed
`tests/two_process_replica_acceptance_test.rs` against PostgreSQL 15 with two
independent `ironcrew serve` operating-system processes, distinct instance
ids, bearer authentication, required idempotency keys, one shared schema, and
one shared HITL keyring. It covers concurrent schema bootstrap, cases 1–3,
keyed portions of 5–8, duplicate-answer rejection, concurrent peer
cancellation, and graceful process cleanup. The CI PostgreSQL job runs this
gate directly; it no longer infers process behavior from two routers in one
runtime.

The associated 150-second local soak completed 253/253 provider-free
cross-replica HITL/SSE runs with zero readiness failures or deadlocks. See the
[reviewed summary and machine report](../evaluations/replica-soak/reports/2026-07-19-local-postgres15-150s.md).

On 2026-07-20, the same test target passed its extended case 11 gate 1/1 in
17.90 seconds against isolated PostgreSQL 15. It observed an active
`WaitingForInput` run, sent `SIGKILL` to and reaped its owner, kept the peer
ready through the real six-second database-clock lease expiry, and observed
peer reconciliation to `Abandoned` with the original run id and owner, a
cleared lease, and an empty HITL mailbox. Same-key retries ran continuously
across the live-lease, expiry, and reconciliation boundaries, followed by four
concurrent post-reconciliation retries; all replayed the original acceptance.
The HTTP run total and exact PostgreSQL run-row/run-event-row counts stayed
unchanged. This proves the retained same-key path does not launch or transfer
execution after owner death; it is not a claim that arbitrary external tool
effects are exactly once.

On 2026-08-07, the current local worktree passed two additional PostgreSQL 15
process gates. IC-005 passed 2/2 in 22.86 seconds: one pair stopped A before
cancellation pickup, and a second held the global idempotency-quota advisory
lock that fences terminal persistence until A had logged cancellation pickup
and PostgreSQL showed its session blocked. Both sent and reaped real `SIGKILL`,
reconciled exactly one run to
`Abandoned`, retained the original keyed acceptance for concurrent replay,
kept zero durable `run_complete` rows, and returned the same unnumbered
incomplete SSE fallback on repeated reads. The completed ledger keeps
`cancel_requested_at` as history while its active lease and HITL mailbox are
empty. The ordinary pre-pickup path sampled B ready before cancellation, after
owner death, and after reconciliation; the artificial global-lock path proved
readiness recovery after the lock was released rather than uninterrupted
readiness during injected database contention.

IC-006 passed 1/1 in 3.89 seconds with
`IRONCREW_REQUIRE_IDEMPOTENCY_KEY=false` on a fresh pair. A provider-free
skipped task created the durable run before process-local HITL. B returned the
exact owner-aware `409` without changing run, ledger, mailbox, or event state;
A retained the same question and renewed its lease. Graceful `SIGTERM` of A
then persisted `Aborted` with the skipped result and one durable terminal
event. B stayed ready, and its follow-up abort returned `404`; both abort audit
rows were failures, so no false success was recorded.

On 2026-08-08, the final IC-017 process gate passed four consecutive focused
runs in 7.69, 7.76, 7.71, and 7.68 seconds against disposable PostgreSQL 15.18.
A accepted and ran the provider-free two-question fixture; B served every
post-start run-event, HITL, and status request. With a four-event per-run limit
and 1 KiB event/page limits, the seven-event journal physically evicted
sequences 1–3 and retained exactly sequences 4–7 across bounded page reads.
B returned the exact authentication-first and malformed/non-ASCII/zero/
non-canonical/cross-run/ahead/expired cursor contracts. A row-scoped database
barrier forced the configured 100 ms read deadline: B returned the exact
non-cacheable `503`, an open stream emitted its fifth-timeout error and closed,
readiness was confirmed after lock release, and a fresh stream replayed
sequence 1. Every successful
stream had `Content-Type: text/event-stream`, `Cache-Control: no-store,
no-transform`, and buffering disabled.

The gate inspected sequences 1–4 before capacity eviction and all seven journal
payloads across the pre- and post-eviction snapshots. It emitted one explicit
`writer_backpressure` gap followed by four unique retained events and resumed
from the boundary without duplicates. After advancing the
disposable rows across their retention deadline, B returned a `retention` gap
through sequence 7 plus one repeatable, unnumbered `run_complete` with
`journal_complete: false`; bounded maintenance then reduced the journal and
global usage to zero rows/bytes. Error bodies and journal/SSE payloads contained
no HITL prompt, choices, answer, API token, idempotency key, or key material.

On 2026-08-09, the IC-019 saturation gate passed four consecutive focused runs
in 6.67, 6.42, 6.63, and 6.41 seconds against disposable PostgreSQL 15.18.
Three sequential pairs of real processes proved that run, conversation, and
SSE semaphores plus work/control/observation buckets are process-local: A
reached each explicit limit while B initially reported zero and retained its
own capacity. A separate one-record ledger pair returned durable `429` through
both replicas, while observation and peer cancellation remained usable. The
two scrape targets exposed different local gauges/counters but identical shared
usage, with no principal, owner, run, key, database, prefix, token, or keyring
material. Direct SQL contained one hashed ledger row and no raw keys.

The complete two-process target passed 6/6 in 59.54 seconds, and the full local
PostgreSQL target set passed 2/2 multi-replica HTTP, 44/44 store, and 6/6
process tests. Cleanup left zero prefixed relations, functions, or replica
backends. The fixture was provider-free and used direct process ports; this is
not cgroup/RSS, nested-flow peak-memory, provider-load, global-admission,
autoscaling/drain, or Railway/OpenShift routing evidence.

On 2026-08-10, the final IC-016 focused process gate passed 1/1 in 9.62 seconds
against freshly pulled PostgreSQL 15.18 after five earlier stabilization
passes. Three provider-free keyed runs moved two real processes through
old-only, expanded/old-active, expanded/new-active, and new-only
configurations. A premature new-only process failed startup while an old-key
question remained and could not mutate the row. Mixed-active delivery produced
old-question/old-answer fingerprints, the owner consumed exactly one answer,
and duplicate delivery returned `404`. After both processes selected the new
active id, the database reported zero old question and answer references; the
new-only peer then listed and answered a new-question/new-answer row exactly
once. SQL ciphertext, shared journal/SSE, audit metadata, HTTP error bodies,
and stopped-process logs exposed no answer, key material, or prohibited prompt.

A live store regression additionally proved that a new-only process whose
startup snapshot preceded a later old-key write rejects the answer without
mutation, that startup checks question and answer fingerprint columns
independently, and that zero retained rows permit safe retirement. The complete
serial PostgreSQL set passed 2/2 multi-replica HTTP, 45/45 store, and 7/7
separate-process tests. Cleanup left zero phase-two relations or replica
sessions. This is same-binary, direct-port local evidence; deployed secret
propagation and load-balanced routing were then gated by IC-007; the
authoritative OpenShift v7 result below closes the OpenShift half.

### Authoritative OpenShift v7 platform evidence — 2026-08-10

The final dirty-worktree v7 artifact passed the complete applicable matrix in a
temporary `restricted-v2` OpenShift deployment. The affinity-free shared Route
returned 64/64 authenticated capability responses across two fully attested
pod/process identities (33/31). Every Ready process independently matched the
exact binary, flow, effective config, keyring, revision, helper, and
build-attestation hashes. The steady profile used lease TTL 60 seconds and a
5000 ms journal-write timeout.

Cases 1–3, 5–8, and 10–13 passed, including counted replay/conflict effects,
encrypted peer HITL, numbered SSE and cursor edges, peer cancellation and its
race, per-pod admission plus shared quota, actual owner `SIGKILL`/`Abandoned`
reconciliation, and explicit drain/replacement. Cases 4 and 9 returned the
required truthful owner-local boundaries. The seven-phase key-rotation rerun
retained 14 complete process inventories, failed premature key retirement
closed without row mutation, reached zero old references, and completed a
new-only peer run. During lifecycle replacement the shared Route passed 180/180
readiness, liveness, and capability probes.

The exact [OpenShift receipt](../evaluations/platform-canary/reports/ic007-openshift-v7.md)
retains security, resource, secret-scan, and cleanup evidence. It also retains
five unfixed HIGH/CRITICAL operating-system findings and the additive
same-namespace NetworkPolicy boundary; no security-clean or exclusive-ingress
claim is made. Both exact canary selectors, database prefixes, and quota use
returned to zero while the shared namespace baseline and authorized OAuth
session were preserved. The unpublished artifact was removed, so its digests
prove observed identity rather than a bit-reproducible or downloadable release.
Specifically, both selectors and all three OpenShift prefixes returned to zero.

### Authoritative Railway v7 rotation evidence — 2026-08-10

Railway's final evidence is cumulative rather than a complete v7 matrix rerun.
Retained earlier receipts cover public route distribution,
replay/conflict/cancellation, encrypted HITL, retained SSE, bounded owner
replacement, and IC-020 lifecycle. V7 supplies the literal remaining IC-007
requirement: expanded/old-active, expanded/new-active, zero old references, and
new-only during real rolling overlap, with exact per-process attestation.

Railway rebuilt the verified ten-file v7 assembly context. Every accepted
process recomputed the same binary, flow, 113-field config, keyring, build
attestation, and helper hashes and matched its authenticated capabilities and
deployment-instance identity. The profile used lease TTL 60 seconds,
journal-write timeout 5000 ms, injected port 8080, a private provider URL, and
an explicitly attested rotation-only supervisor.

One retained snapshot contained exactly two expanded/old-active and two
expanded/new-active processes. A new-active process answered the old-owned run,
an old-active process answered the new-owned run, both second questions were
also peer-answered, both owners reached `Success`, and both journals converged
to the seven-event terminal barrier. A transaction-scoped observer retained
only ciphertext lengths/hashes, proved one old and one new fingerprint with no
fixed plaintext, and was removed by exact OID. Old question/answer/union
references were `0|0|0` before retirement. The final two-process new-only peer
run also completed `Success` with an empty mailbox and the same barrier.

The overlap peak was four app processes; the new-only transition briefly
reached six before predecessor removal. At one CPU, 1 GB, and pool size two per
process, those are declared ceilings of 4/6 vCPU, 4/6 GB, and 8/12 PostgreSQL
connections—not sustained usage or OOM proof. Bounded accepted-window scans
found no exact credential/key match or semantic error-keyword line, but no
security-clean claim is made and historical v5 negative logs remain.

The current [Railway receipt](../evaluations/platform-canary/reports/ic007-railway-v7.md)
reports exact cleanup of six prefixes, the domain, three services, all active
instances/volumes/proxies/buckets, and attributable local staging, scratch,
cache, container, and image objects. It preserves the sandbox project,
environment, two pre-existing volume tombstones, and `postgres:15`; it used no
broad delete or prune. Independent final audit confirmed those zero baselines
and exact preserved resources. The artifact copies were removed, so the
retained digests prove observed identity rather than availability or bit
reproducibility.

### Interim platform canary evidence — 2026-08-10

Two disposable provider-free deployments used the same dirty-worktree Linux
binary and flow, shared PostgreSQL per platform, required idempotency keys, and
two 1-CPU/1-GiB application replicas. A Railway public route observed exactly
two generated process identities over 64 capability samples (34/30); its short
v2 target run passed 8/8 load-balanced HITL, run-read, and initial/reconnected
SSE cases and independently sampled those identities 37/27. Identical-key
replay, conflicting-key reuse, route-level cancellation, authentication
negatives, and protected metrics matched the documented contracts through the
route; the receiving replica for cancellation was not attributable. Railway
platform metrics sampled 0.00724 vCPU and 9.40 MB at peak during the canary, but
no host RSS was available and this was not a sustained resource-ceiling run.

An affinity-disabled OpenShift Route observed both pod-UID identities over 64
samples (25/39); its short v2 target run passed 20/20 corresponding cases. Both
digest-pinned pods ran under `restricted-v2` as UID `1004800000`. A real
`SIGKILL` of an identified non-restarting owner produced exit 137 while the
peer stayed ready, reconciled the original run to `Abandoned`, cleared its
mailbox, and left exact run/event/idempotency counts unchanged through five
same-key replays. The staged HITL rollout passed expanded/old-active,
mixed-active, new-active, zero-old-reference, and new-only revisions, followed
by a successful new-only cross-replica HITL run.

Bounded platform-log and scoped-database scans found none of the exact canary
secret values or selected credential patterns. All attributable active Railway
and OpenShift resources and table prefixes were removed; at that interim
checkpoint, Railway's volume deletion still appeared asynchronously pending in
its deleted-resource record. The
Railway owner-to-deployment-instance mapping and staged rotation were not
proven, and neither platform exercised IC-020's explicit drain/scale
lifecycle. These are temporary routing canaries, not provider-load, execution
takeover, exactly-once external-effects, long-duration soak, or complete RAM-limit
evidence. Owner-local conversation and unkeyed-control delivery remain outside
the shared contract; case 4 proves truthful diagnosis only, not routing.
