# IronCrew REST API

IronCrew includes a built-in REST API server that lets you run crew flows over HTTP,
stream execution events via SSE, and manage run history.

## Starting the Server

```bash
ironcrew serve --flows-dir ./flows --port 3000
```

| Flag           | Default       | Description                          |
|----------------|---------------|--------------------------------------|
| `--host`       | `127.0.0.1`   | Host address to bind to              |
| `--port`       | `3000`        | Port to bind to                      |
| `--flows-dir`  | `.`           | Directory containing crew flow dirs  |

The server loads `.env` from the current working directory **once at startup**
(before the async runtime starts), so API keys and config set there are available
to every flow. In server mode a flow's own `.env` file is **not** loaded — flows
read the shared process environment. Set per-deployment secrets at the process
level (container env, systemd, the server's CWD `.env`), not in per-flow `.env`
files. (This replaces the earlier per-request loading, which raced on the
environment and could bleed one flow's secrets into another.)

For production sizing, session-cap tuning, SSE proxy guidance, and horizontal
scaling considerations, see [HTTP Scaling](http-scaling.md). The exact shared
and process-local boundary is documented in the
[Multi-Replica Deployment Contract](multi-replica.md).

## Endpoints

| Method   | Path                                               | Description                                         |
|----------|----------------------------------------------------|-----------------------------------------------------|
| GET      | `/health`                                          | Health check (returns version)                      |
| GET      | `/capabilities`                                    | Authenticated instance/control-scope diagnostics    |
| POST     | `/flows/{flow}/run`                                | Start a crew run (async)                            |
| POST     | `/flows/{flow}/abort/{run_id}`                     | Abort a running crew                                |
| GET      | `/flows/{flow}/events/{run_id}`                    | SSE event stream for a run                          |
| GET      | `/flows/{flow}/questions/{run_id}`                 | Pending `ask_human` questions for a suspended run   |
| POST     | `/flows/{flow}/answer/{run_id}`                    | Answer a pending `ask_human` question               |
| GET      | `/flows/{flow}/runs`                               | List past runs for a flow                           |
| GET      | `/flows/{flow}/runs/{id}`                          | Get a specific run record                           |
| DELETE   | `/flows/{flow}/runs/{id}`                          | Delete a run record                                 |
| GET      | `/flows/{flow}/validate`                           | Validate a flow (syntax + agents)                   |
| GET      | `/flows/{flow}/agents`                             | List agents defined in a flow                       |
| GET      | `/nodes`                                           | List all built-in tools                             |
| POST     | `/flows/{flow}/conversations/{id}/start`           | Create or re-open a chat session — see [chat.md](chat.md) |
| POST     | `/flows/{flow}/conversations/{id}/messages`        | Send a user turn, wait for a reply — see [chat.md](chat.md) |
| GET      | `/flows/{flow}/conversations/{id}/history`         | Read the stored transcript — see [chat.md](chat.md) |
| GET      | `/flows/{flow}/conversations/{id}/events`          | SSE stream for a chat session — see [chat.md](chat.md) |
| DELETE   | `/flows/{flow}/conversations/{id}`                 | Drop handle + delete record — see [chat.md](chat.md) |
| GET      | `/flows/{flow}/conversations`                      | Paginated list of chat sessions — see [chat.md](chat.md) |

The `{flow}` parameter is the directory name inside `--flows-dir`. Path traversal
is rejected -- only single-component names are accepted.

## Running a Flow

`POST /flows/{flow}/run` launches execution in the background and returns immediately
with a `run_id`. The JSON request body (if any) is injected as a global `input` table
in Lua.

```bash
curl -X POST http://localhost:3000/flows/research-crew/run \
  -H "Content-Type: application/json" \
  -d '{"topic": "quantum computing", "depth": "brief"}'
```

Response:

```json
{
  "run_id": "a1b2c3d4-...",
  "status": "started",
  "events_url": "/flows/research-crew/events/a1b2c3d4-...",
  "owner_instance_id": "ironcrew-1234-...",
  "control_scope": "process"
}
```

The `run_id` is consistent across the initial response, SSE events, and the
persisted run record. `owner_instance_id` identifies the process that owns the
live Lua execution; it is diagnostic metadata, not a routable URL.
`control_scope: "process"` refers to execution ownership. PostgreSQL exposes a
separate shared run-event journal, and a run started with an `Idempotency-Key`
can also use the shared HITL mailbox when the deployment has a
[human-input keyring](#cross-replica-delivery). Use `GET /capabilities` rather
than the acceptance field to discover each control surface.

### Runtime capabilities

`GET /capabilities` is protected by the normal API authentication policy and
returns `Cache-Control: no-store`. It reports the current instance id and the
scope of each live-control surface. `multi_replica_control: false` means not
all run/HITL/SSE/conversation controls can enter through an arbitrary replica;
it remains `false` even when PostgreSQL supports shared SSE replay and keyed
runs support bounded cross-instance cancellation/encrypted HITL delivery. Inspect
`live_control.human_input`: `"shared_store_for_keyed_runs"` means the keyring
is active, while `"process"` means questions still require the owner replica.
`live_control.sse_replay` is `"shared_store"` for PostgreSQL and `"process"`
for JSON/SQLite.

An optional top-level `tags` array is attached to the run record. It accepts
unique, non-empty, trimmed strings without control characters. The defaults
are at most 32 tags, 256 bytes per tag, and 4096 aggregate tag bytes; operators
can lower those policies with `IRONCREW_API_MAX_TAGS`,
`IRONCREW_API_MAX_TAG_BYTES`, and `IRONCREW_API_MAX_TAGS_BYTES` (hard ceilings
256, 4096 bytes, and 65536 bytes respectively).

Each run has a maximum lifetime (default: 30 minutes). If execution exceeds this
limit, the run is aborted and a `run_complete` event is emitted with `status: "timeout"`.
Configure via `IRONCREW_MAX_RUN_LIFETIME` env var (seconds).

### Safe retries with `Idempotency-Key`

`POST /flows/{flow}/run` and
`POST /flows/{flow}/conversations/{id}/messages` accept one optional
`Idempotency-Key` header. Production deployments should set
`IRONCREW_REQUIRE_IDEMPOTENCY_KEY=true`, which makes the header mandatory on
both endpoints. A key must contain 1–128 visible ASCII bytes with no
whitespace. IronCrew hashes it before persistence; the raw key is never stored
or written to the audit log. Use an unguessable value (for example, a UUIDv4
or 128-bit random token), because retaining the prior raw key is also the
capability used for explicit indeterminate-turn recovery.

```bash
curl -X POST http://localhost:3000/flows/research-crew/run \
  -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: client-job-018f6c' \
  -d '{"topic":"quantum computing"}'
```

The first request allocates its run id, but IronCrew does not return or replay
the `started` response until the matching durable run intent exists and the
exact fenced claim is `running`. A retry with the same key and the same
canonical request returns the original status and JSON body, including the
same `run_id`, with
`Idempotency-Replayed: true`. Object-key order does not change a fingerprint;
array order does. A missing run body is distinct from explicit JSON `null`.
For messages, an absent, `null`, or empty `images` list is equivalent.

- Reusing a key for another endpoint, flow, conversation, or request body
  returns `409 Conflict`.
- A matching message that is still executing returns `409`; it is not queued.
- If another pod has already advanced the durable conversation revision, the
  stale local handle is discarded and the request returns `409`; call
  `/start` to reload before retrying.
- If a pod dies after a message may have invoked a provider or tool, the key
  becomes an indeterminate tombstone and returns `409` instead of executing
  those effects again. A different key cannot silently bypass an expired
  conversation claim: IronCrew first installs a scope hazard and returns
  `409`. Inspect conversation history, then deliberately acknowledge the old
  key while submitting a new key:

  ```bash
  curl -X POST http://localhost:3000/flows/chat/conversations/customer-42/messages \
    -H 'Content-Type: application/json' \
    -H 'Idempotency-Key: message-attempt-2' \
    -H 'Idempotency-Recovery-Key: message-attempt-1' \
    -d '{"content":"Continue after the inspected turn"}'
  ```

  The recovery header must contain the prior raw key; IronCrew hashes it and
  atomically consumes only the matching conversation hazard. If the request
  is the one that first discovers the expired worker, it returns one `409`
  barrier. Even a correctly acknowledged replacement remains blocked for one
  full `IRONCREW_RUN_LEASE_TTL_SECONDS` interval after that hazard is recorded,
  preventing overlap with a stale worker that is still unwinding. Retry the
  identical recovery request after that grace interval (60 seconds by
  default); earlier retries continue to return `409`.
  Re-open an evicted/restarted conversation with `/start` before recovery.
- A crashed run claim is reconciled to an `abandoned` run with its original
  `run_id`; retries continue to return that original acceptance response.
- While a keyed run executes, one fenced heartbeat atomically renews both its
  run lease and operation-ledger lease. Losing either fence stops further Lua,
  provider, and tool work before terminal reconciliation proceeds.
- Client disconnect does not cancel a keyed message. The server-owned task
  finishes its atomic transcript/replay commit in the background.

The guarantee is completed-response replay and no automatic duplicate
execution during retention. It cannot make arbitrary external tools
transactional. Keep tool operations independently idempotent when possible.
Ledger rows expire after `IRONCREW_IDEMPOTENCY_TTL_SECONDS`; terminal responses
are bounded by per-record and aggregate byte caps. See
[Storage Backends](storage.md#idempotency-ledger) for all limits and backend
semantics.

## Aborting a Run

Cancel a running crew by calling the abort endpoint:

```bash
curl -X POST http://localhost:3000/flows/my-crew/abort/abc-123
# {"run_id":"abc-123","status":"aborted"}
```

That immediately cancels all in-flight LLM calls and drops pending tasks when
the request reaches the owner process. The SSE stream receives a `run_complete`
event with `status: "aborted"`, and pending human-input questions expire before
the local abort response returns. The run's event bus remains available for
late SSE recovery and is cleaned up after 5 seconds.

For a PostgreSQL-backed run created with an `Idempotency-Key`, another replica
can durably request cancellation through the shared ledger. Its `200` response
has `status: "cancellation_requested"`, `control_scope: "shared_store"`, the
owner id, and an `already_requested` flag. That transaction also removes the
run's pending encrypted human-input rows before it acknowledges the request.
The owner observes the cancellation on its fenced heartbeat, stops the worker,
and then persists `aborted`; therefore the cancellation response is an
acknowledgement, not proof that the run is already terminal.

If the owner dies after that acknowledgement but before its terminal
compare-and-set commits, lease reconciliation records `abandoned`, not
`aborted`. No durable `run_complete` event is invented for the dead process;
PostgreSQL SSE recovery instead emits one unnumbered fallback completion with
`journal_complete: false` and `synthesized_from_run_record: true`.

If an active run belongs to another instance and the configured backend has no
durable cancellation mailbox (including JSON, SQLite, or an unkeyed run), the
endpoint returns `409` with `code: "run_owned_by_another_instance"` and
`retryable: true`. An in-flight durable record owned by this instance but
missing its local control handle returns retryable `503`. Missing, terminal,
and cross-flow runs remain `404` where flow isolation requires it.

A run can end with one of these statuses:
- `success` / `partial_failure` / `failed` — normal completion
- `timed_out` — configured maximum lifetime exceeded
- `aborted` — cancelled via this endpoint
- `abandoned` — the owning process disappeared before terminal persistence

## Mid-Run Questions (`crew:ask_human`)

When a flow calls [`crew:ask_human()`](crews.md#human-in-the-loop-ask_human),
the run suspends until a human answers over these endpoints (or the question
times out). The SSE stream carries a `human_input_requested` event the moment
the question is asked, so a UI can render a form immediately; poll-only
clients recover the same state from the questions endpoint.

### List pending questions

```bash
curl http://localhost:3000/flows/my-crew/questions/abc-123
```

```json
{
  "run_id": "abc-123",
  "status": "waiting_for_input",
  "owner_instance_id": "ironcrew-pod-a",
  "control_scope": "shared_store",
  "questions": [
    {
      "question_id": "9c1e…",
      "prompt": "Deploy to production?",
      "choices": ["yes", "no", "staging only"],
      "asked_at": "2026-07-07T10:00:00Z",
      "timeout_s": 600,
      "kind": "question"
    }
  ]
}
```

`status` is `"running"` with an empty array when nothing is pending. Both
question endpoints return `Cache-Control: no-store`. `control_scope` is
`"shared_store"` for the configured PostgreSQL mailbox and `"process"` for
owner-local delivery.

Without the [cross-replica prerequisites](#cross-replica-delivery), a live run
owned by another instance returns the structured retryable `409` described
above; the question bridge has not moved to that replica. A same-owner durable
record with no local bridge returns retryable `503`. Missing, terminal, and
cross-flow runs return `404`, so the endpoint never confirms that a run exists
under a different flow.

`kind` is `"question"` (from `crew:ask_human()` or the agent-facing
`ask_human` tool) or `"approval"` (from a
[tool approval gate](crews.md#approval-gates-require_approval)) — render an
answer form for the first, allow/always/deny buttons for the second. For
approvals, only the standalone tokens `allow`/`yes`/`always`/`allow-always`
permit the call after case-folding and trimming surrounding whitespace;
anything else denies (free text is forwarded to the agent as the denial
reason).

### Answer a question

```bash
curl -X POST http://localhost:3000/flows/my-crew/answer/abc-123 \
  -H 'Content-Type: application/json' \
  -d '{"question_id": "9c1e…", "answer": "yes"}'
# HTTP 202
# {"run_id":"abc-123","question_id":"9c1e…","status":"queued",
#  "owner_instance_id":"ironcrew-pod-a","control_scope":"shared_store"}
```

`answer` may be any JSON value — a string, number, or a whole object; the
suspended flow receives it as a Lua value. First writer wins: a second answer
to the same question gets `404` (the question is gone), never a silent
overwrite. `choices` are advisory — the endpoint accepts free-form answers
even when choices were offered; flows that need strict values validate in Lua.
An answer larger than `IRONCREW_ASK_HUMAN_MAX_ANSWER_BYTES` (65,536 serialized
bytes by default, configurable up to the 1,048,576-byte hard maximum) gets
`413 Payload Too Large` and leaves the question pending so the client can retry
with a smaller value.

A shared-mailbox answer returns `202 Accepted` and `status: "queued"` after
PostgreSQL has accepted the encrypted value. The owner polls the mailbox and
resumes the suspended coroutine; `202` is not proof that Lua has already
consumed the answer. An owner-local or non-durable answer instead returns
`200 OK` and `status: "delivered"`. In both modes, the first accepted writer
wins and subsequent attempts return `404`.

### Cross-replica delivery

Arbitrary replicas can list and answer a pending question only when all of the
following are true:

- the run was started over HTTP with an `Idempotency-Key`;
- every replica uses the same PostgreSQL database and table prefix;
- every replica has the same `IRONCREW_HITL_ENCRYPTION_KEYS` JSON keyring and
  a valid `IRONCREW_HITL_ACTIVE_KEY_ID`.

The keyring is a JSON object whose values are canonical base64 encodings of
32-byte keys. It accepts at most eight keys and 16 KiB of JSON. Question
prompt/choices/timing metadata and answers are encrypted with AES-256-GCM
before PostgreSQL persistence. Flow, run, question, owner, attempt, key digest,
timestamps, and encryption-key fingerprint remain routing/fencing metadata in
the database. Ciphertext authentication binds an answer to that exact run,
question, owner, idempotency attempt, and key digest.

The owner checks PostgreSQL once per pending question every
`IRONCREW_HITL_POLL_INTERVAL_MS` (default `500` ms, minimum `50`, hard maximum
`5000`). Each read has `IRONCREW_HITL_READ_TIMEOUT_MS` (default `2000` ms,
effective range `100`–`30000`). Pending work is bounded by
`IRONCREW_ASK_HUMAN_MAX_PENDING` (default `16`, hard maximum `256`) and
`IRONCREW_ASK_HUMAN_MAX_PENDING_BYTES` (default 1 MiB, hard maximum 16 MiB) per
run. Prompt, choice, timeout, and serialized answer caps still apply; see
[the CLI environment reference](cli.md#environment-variables).
At the defaults, one run parked on all 16 allowed questions performs about 32
mailbox reads per second, so increase the interval or lower the pending cap
before multiplying that workload across many Railway/OpenShift replicas.
The default prompt plus aggregate-choice budget is 128 KiB of raw text per
pending question, but the aggregate serialized metadata cap prevents 16 such
questions from accumulating when they exceed 1 MiB together. PostgreSQL
ciphertext admission additionally accounts for 28 AEAD bytes per allowed row.
Keep the aggregate cap conservative on small pods.

Concurrent PostgreSQL question-list/decrypt operations are bounded per process
by `IRONCREW_HITL_PG_MAX_CONCURRENT_READS` (default `8`, range `1`–`64`). The
question-list endpoint is also subject to the process-local per-principal
observation bucket (`IRONCREW_ADMISSION_OBSERVATION_RATE_PER_MINUTE=600`,
`IRONCREW_ADMISSION_OBSERVATION_BURST=20` by default); neither limit throttles
the owner's internal answer polling.

Keep the keyring in a Railway variable, OpenShift/Kubernetes `Secret`, or an
external secret manager. Rotate it in two deployments: first add the new key
to every replica while the old key remains active; then select the new active
key everywhere while retaining the old key until no mailbox row references its
question or answer fingerprint. Answer consumption, timeout, terminalization,
and abandoned-run reconciliation normally clear those rows. Removing an old
key based only on wall-clock age can strand an answer queued during an owner
outage. Never place the keyring in the image or a checked-in manifest.

This mailbox routes a command to the current owner; it does **not** move or
recreate the Lua VM. It does not provide execution takeover after owner death
or share live conversation handles. PostgreSQL SSE replay is a separate
plaintext, bounded journal; the HITL keyring does not encrypt it. See the
[Multi-Replica Deployment Contract](multi-replica.md) for the complete boundary.

Both endpoints are recorded in the [audit log](#get-audit) (actions
`flow.run.questions_list` / `flow.run.question_answer`). The bridge-generated
audit records and `human_input_*` SSE events carry the `question_id` but never
the answer content. This does not sanitize arbitrary flow logs, model output,
or tool output: flows and prompts must not echo sensitive answers themselves.

Locally aborting a suspended run expires its pending questions before the
response returns. A durable cross-instance cancellation atomically records the
request and removes its pending encrypted question rows before returning
`200`; the owner may observe and terminalize that request later. Once terminal,
both question endpoints return `404`; the separate owner-local SSE event bus
may remain available for JSON/SQLite during its short retention window, while
PostgreSQL replay follows the durable journal contract below.

## SSE Event Stream

`GET /flows/{flow}/events/{run_id}` returns a Server-Sent Events stream.

```bash
curl -N http://localhost:3000/flows/research-crew/events/a1b2c3d4-...
```

Successful SSE responses use `Cache-Control: no-store, no-transform` and
`X-Accel-Buffering: no`; validation/error responses are also non-cacheable.
These streams can contain model output, reasoning, tool/log text, and other
sensitive run data.

### PostgreSQL replay and `Last-Event-ID`

PostgreSQL-backed HTTP runs write a bounded event journal that any replica can
read while the run is active or terminal. Every retained event has a canonical
SSE id in the form `<run_id>:<sequence>`, for example:

```text
id: a1b2c3d4-...:17
event: task_completed
data: {"event":"task_completed","data":{...}}
```

Reconnect with the last id the client fully processed:

```bash
curl -N \
  -H 'Last-Event-ID: a1b2c3d4-...:17' \
  http://localhost:3000/flows/research-crew/events/a1b2c3d4-...
```

The sequence is a positive canonical decimal integer (no leading zeroes), and
the run id in the header must equal the path run id. IronCrew resumes strictly
after that sequence. Browsers using `EventSource` send the last received SSE
id automatically when they reconnect.

The journal is bounded, so replay can be incomplete. Without a cursor, a
subscriber receives a `journal_gap` event before the next retained event when
an earlier sequence range was omitted or evicted. Its data includes
`first_sequence`, `last_sequence`, and one reason:
`writer_backpressure`, `retention`, `global_capacity`, or `owner_lost`. The gap
event's own SSE id is `<run_id>:<last_sequence>`; persist that id just like a
normal event before continuing. A cursor older than the retained boundary is
rejected instead of silently skipping data.

Cursor failures are deterministic:

| Condition | Status | Code/meaning |
|---|---:|---|
| malformed, non-ASCII, zero/non-canonical sequence | `400` | `invalid_cursor` |
| cursor belongs to another run | `400` | `cursor_cross_run` |
| sequence is newer than the journal | `409` | `cursor_ahead` |
| sequence is older than the retained boundary | `409` | `cursor_expired` |
| `Last-Event-ID` used with JSON or SQLite | `409` | shared replay is unavailable |

Active-stream completeness is best-effort. The producer uses bounded queues,
bytes, batches, retries, and deadlines; saturation or a database outage can
create an explicit gap while the authoritative run continues. A normal
persisted `run_complete` has a sequence/id and closes the stream. If that event
was omitted or physically pruned but the run record is terminal, IronCrew
synthesizes an unnumbered `run_complete` from the durable run record with
`journal_complete: false` and `synthesized_from_run_record: true`, then closes.
That fallback proves terminal state, not complete event history, and provides
no cursor to acknowledge. After five consecutive journal read failures or
timeouts, the stream emits an SSE `error` event and closes so the client can
retry with its last fully processed id.

JSON and SQLite keep the earlier process-local behavior: late subscribers on
the owner receive its bounded in-memory replay and then live broadcasts. A
foreign replica cannot reconstruct that stream, and `Last-Event-ID` receives
`409`. A terminal run record can still produce one unnumbered completion, but
not the missing history.

PostgreSQL stores journal payloads as **plaintext JSONB**, not with the HITL
encryption keyring. Most events retain their normal data, including potentially
sensitive task/model/tool/log content. The durable `human_input_requested`
form is the exception: it omits prompt and choices, includes the question id
and authenticated questions endpoint, and requires the client to fetch the
encrypted mailbox metadata separately. `human_input_received` never includes
the answer. Because IronCrew currently has authentication but no per-flow
read authorization, every configured API bearer token is effectively an
administrator credential for run events; do not issue tokens directly to
untrusted end users.

The journal's count/byte settings are logical retention controls, not a
PostgreSQL disk quota. Accounted bytes use at least 1 KiB per event and cover
the JSON payload, but exclude tuple/page overhead, indexes, state/usage rows,
WAL, replicas/backups, and dead tuples pending vacuum. Monitor actual database
size and autovacuum in addition to these limits. See
[Storage Backends](storage.md#bounded-postgresql-run-event-journal).

### Output Truncation

By default, SSE events can include full task output up to the event-size cap.
For process-local JSON/SQLite streams, set
`IRONCREW_SSE_OUTPUT_MAX_CHARS` to further cap the output field in
`task_completed` and `collaboration_turn` events:

```bash
IRONCREW_SSE_OUTPUT_MAX_CHARS=500 ironcrew serve --flows-dir ./flows
```

When truncated, the output ends with `... [truncated, N total bytes]`.
This response-only setting does not rewrite PostgreSQL journal rows. Durable
events are instead bounded before persistence by `IRONCREW_EVENT_MAX_BYTES`;
flows should avoid emitting secrets regardless of either cap. Run history and
the `/flows/{flow}/runs/{id}` endpoint retain their independently bounded task
results.

### Event Types

| Event                | Fields                                                        | Description                                  |
|----------------------|---------------------------------------------------------------|----------------------------------------------|
| `crew_started`       | `goal`, `agent_count`, `task_count`, `model`                  | Crew execution begins                        |
| `phase_start`        | `phase`, `tasks`                                              | A new execution phase starts                 |
| `task_assigned`      | `task`, `agent`, `phase`                                      | Task assigned to an agent                    |
| `task_completed`     | `task`, `agent`, `duration_ms`, `success`, `output`, `token_usage` | Task finished successfully              |
| `task_failed`        | `task`, `agent`, `error`, `duration_ms`                       | Task execution failed                        |
| `task_skipped`       | `task`, `reason`                                              | Task skipped (condition evaluated false)     |
| `task_thinking`      | `task`, `agent`, `content`                                    | Model reasoning/thinking (Anthropic, OpenAI Responses, DeepSeek, Kimi) |
| `task_retry`         | `task`, `attempt`, `max_retries`, `backoff_secs`, `error`     | Task being retried after failure             |
| `tool_call`          | `task`, `tool`                                                | Agent invoked a tool                         |
| `tool_result`        | `task`, `tool`, `success`, `duration_ms`                      | Tool returned a result                       |
| `agent_tool_started` | `caller`, `callee`, `prompt`                                  | Fires immediately before a sub-agent runs via `agent__<name>`. `caller` and `callee` are bare agent names. Used by UIs to scope the nested `tool_call` / `tool_result` events under a single marker. |
| `agent_tool_completed` | `caller`, `callee`, `duration_ms`, `success`                | Fires when the sub-agent returns. `success` is `false` only if the invocation errored at the Rust/provider level — a sub-agent that returned a low-quality reply still counts as `success: true`. |
| `collaboration_turn` | `task`, `agent`, `turn`, `content`                            | A turn in a collaborative task               |
| `conversation_started` | `conversation_id`, `agent`                                  | A `crew:conversation()` was created          |
| `conversation_turn`  | `conversation_id`, `agent`, `turn_index`, `user_message`, `assistant_message` | Single completed turn (`send`/`ask`) |
| `conversation_thinking` | `conversation_id`, `agent`, `turn_index`, `content`         | Reasoning captured during a conversation turn |
| `dialog_started`     | `dialog_id`, `agents`, `max_turns`                            | A `crew:dialog()` was created (`agents` is the array of participating agent names in turn order) |
| `dialog_turn`        | `dialog_id`, `turn_index`, `speaker`, `agent`, `content`      | One turn in an agent-to-agent dialog (`speaker` = "a" or "b") |
| `dialog_thinking`    | `dialog_id`, `turn_index`, `speaker`, `agent`, `content`      | Reasoning captured during a dialog turn      |
| `dialog_completed`   | `dialog_id`, `total_turns`, `stop_reason?`                    | Dialog ended (either reached `max_turns` or a `should_stop` callback stopped it; `stop_reason` is present only when the callback stopped it) |
| `message_sent`       | `from`, `to`, `message_type`                                  | Inter-agent message sent                     |
| `memory_set`         | `key`                                                         | A memory key was written                     |
| `human_input_requested` | local: `question_id`, `prompt`, `choices`, `timeout_s`, `kind`; durable: `question_id`, `timeout_s`, `kind`, `question_method`, `question_endpoint`, `question_metadata` | The run suspended on a human question. PostgreSQL replay deliberately uses `question_metadata: "omitted_from_event_journal"`; GET the authenticated questions endpoint to recover encrypted prompt/choices. |
| `human_input_received` | `question_id`, `outcome`                                    | The question resolved (`outcome`: `"answered"` or `"timeout"`). Never carries the answer content — answers may contain secrets |
| `journal_gap`        | `first_sequence`, `last_sequence`, `reason`                  | PostgreSQL replay omitted/evicted a sequence range. Its SSE id advances through `last_sequence`; do not infer events inside the gap. |
| `log`                | `level`, `message`                                            | General log entry (info, error, etc.)        |
| `run_complete`       | `run_id`, `status`, `duration_ms`, `total_tokens`             | Run finished (terminal event)                |

The `token_usage` field in `task_completed` contains:

```json
{
  "prompt_tokens": 150,
  "completion_tokens": 42,
  "total_tokens": 192,
  "cached_tokens": 0
}
```

A process-local `warning` event may be sent if a live subscriber falls behind
its broadcast channel. PostgreSQL replay represents durable omissions with
`journal_gap` instead.

### Conversation and Dialog Events

`crew:conversation({})` and `crew:dialog({})` emit dedicated SSE events with
stable identifiers (`conversation_id` / `dialog_id`) so clients can group
events per primitive when multiple are running in the same `crew:run()`.

**Conversation lifecycle:**
- `conversation_started` — at construction
- `conversation_turn` — once per `send()` / `ask()` call (with both the user
  and assistant messages)
- `conversation_thinking` — once per turn when the provider returns reasoning
  content (Anthropic, OpenAI Responses, DeepSeek, Kimi)

**Dialog lifecycle:**
- `dialog_started` — at construction
- `dialog_turn` — once per turn (one event per `next_turn()` or per turn
  inside `run()`)
- `dialog_thinking` — once per turn when reasoning is captured
- `dialog_completed` — emitted exactly once, either when the dialog reaches
  `max_turns` or when a `should_stop` Lua callback requests early termination.
  The event carries an optional `stop_reason` string in the early-stop case
  (omitted for max-turns completion, so older clients are unaffected)

Conversation and dialog output also still streams to stderr in the Lua process
(with dim styling for reasoning) — the SSE events are an additional channel.

## Operational Notes

For HTTP chat deployments, keep in mind:

- `IRONCREW_MAX_ACTIVE_CONVERSATIONS` caps live in-memory chat handles, not
  total persisted sessions and not overall throughput
- `IRONCREW_MAX_CONVERSATION_LIFECYCLES` caps distinct conversation IDs with
  an in-flight start, message, delete, or eviction operation (default `256`,
  hard ceiling `4096`). New operations for a different ID return `503` while
  the cap is full; an existing ID keeps its per-conversation serialization.
  Entries are removed as soon as their last operation finishes, so arbitrary
  IDs do not accumulate in process memory
- `IRONCREW_CHAT_SESSION_IDLE_SECS` controls when inactive chat handles are
  evicted from RAM
- SSE replay is bounded separately by `IRONCREW_MAX_EVENTS` and
  `IRONCREW_EVENT_REPLAY_MAX_BYTES` (default 4 MiB). Conversation and
  JSON/SQLite run replay consume process memory; PostgreSQL run replay uses the
  bounded journal plus transient per-run producer queues and per-reader pages

For tuning guidance and deployment patterns, see [HTTP Scaling](http-scaling.md).

## Run History

### List Runs

Paginated, metadata-only listing of past runs. The response body is an
object with `runs`, `total`, `limit`, and `offset` — **not** a bare array.
Individual run summaries omit `task_results` so listings stay cheap even on
stores with thousands of historical runs.

Results are **scoped to the flow in the URL**: `GET /flows/A/runs` returns only
runs launched under flow `A`, and `GET`/`DELETE /flows/A/runs/{id}` act only on
a run belonging to `A` (a run from another flow reads as `404`). Runs recorded
before this scoping was introduced carry no flow tag and are not visible through
the per-flow endpoints.

```bash
# First page (defaults: 20 per page, newest first)
curl http://localhost:3000/flows/research-crew/runs

# Filter by status
curl "http://localhost:3000/flows/research-crew/runs?status=success"

# Filter by tag and limit
curl "http://localhost:3000/flows/research-crew/runs?tag=prod&limit=50"

# Page 3 (skip first 40)
curl "http://localhost:3000/flows/research-crew/runs?limit=20&offset=40"

# Only runs since a given RFC3339 timestamp
curl "http://localhost:3000/flows/research-crew/runs?since=2026-03-01T00:00:00Z"
```

**Query parameters**

| Param    | Type    | Description |
|----------|---------|-------------|
| `status` | string  | `success`, `partial_failure`, `failed`, `aborted`, `timed_out`, `running`, `waiting_for_input`, `abandoned` |
| `tag`    | string  | Exact-match against the run's tag list |
| `since`  | string  | RFC3339 timestamp; only runs at or after this time |
| `limit`  | integer | Page size (default `IRONCREW_RUNS_DEFAULT_LIMIT`, capped at `IRONCREW_RUNS_MAX_LIMIT`, default 100) |
| `offset` | integer | Skip the first N results |

**Response shape**

```json
{
  "runs": [
    {
      "run_id": "a1b2c3d4-...",
      "flow_name": "research-crew",
      "status": "success",
      "started_at": "2026-04-09T08:00:00Z",
      "finished_at": "2026-04-09T08:01:20Z",
      "duration_ms": 80000,
      "agent_count": 2,
      "task_count": 3,
      "total_tokens": 1200,
      "cached_tokens": 400,
      "tags": ["prod"]
    }
  ],
  "total": 137,
  "limit": 20,
  "offset": 0
}
```

To fetch a full `RunRecord` (including `task_results`), call
`GET /flows/{flow}/runs/{id}`.

During a sustained terminal-persistence outage, IronCrew prioritizes bounded
server memory and durable terminal metadata over retaining large process-local
outputs indefinitely. The full task results receive their normal write attempt;
payloads no larger than 1 MiB receive one additional full attempt. If those
writes fail, status, timing, and aggregate token counts continue retrying after
the staged task results are released, so a recovered terminal record can have
an empty `task_results` array. See [Cloud Deployment](cloud-deployment.md) for
the Railway/OpenShift operational rationale.

### Get Run Details

```bash
curl http://localhost:3000/flows/research-crew/runs/a1b2c3d4-...
```

Returns a full `RunRecord` with task results, token counts, and timing.

### Delete a Run

```bash
curl -X DELETE http://localhost:3000/flows/research-crew/runs/a1b2c3d4-...
```

### Storage Backend

Run history uses the configured storage backend. By default, runs are stored
as JSON files. Set `IRONCREW_STORE=sqlite` for SQLite:

```bash
IRONCREW_STORE=sqlite ironcrew serve --flows-dir ./flows
```

All run history endpoints (`list_runs`, `get_run`, `delete_run`) work
identically regardless of backend. Under `ironcrew serve`, the store is a
**server-wide singleton** bootstrapped once at startup in `cmd_serve`
(`src/cli/server.rs`) and shared across every flow and every request. This
means Postgres bootstrap runs exactly once, the connection pool is shared,
and per-request `/start` latency stays around ~10 ms instead of the
~300 ms a per-request bootstrap would cost.

## Flow Inspection

### Validate a Flow

```bash
curl http://localhost:3000/flows/research-crew/validate
```

Returns:

```json
{
  "flow": "research-crew",
  "valid": true,
  "agents": [
    { "name": "researcher", "goal": "...", "capabilities": [...], "tools": [...] }
  ],
  "custom_tools": ["summarize"],
  "entrypoint": "/path/to/crew.lua"
}
```

### List Agents

```bash
curl http://localhost:3000/flows/research-crew/agents
```

Returns agent definitions including `name`, `goal`, `capabilities`, `tools`,
`temperature`, and `model`.

### List Built-in Tools

```bash
curl http://localhost:3000/nodes
```

Returns all registered built-in tools with their names, descriptions, and
JSON Schema parameter definitions.

## Conversations (Phase 1 Human-in-the-Loop)

Phase 1 exposes the existing `crew:conversation({...})` primitive as six
HTTP endpoints. Sessions are created explicitly with `POST /start`, turns
are serialized per-id, and records persist through the same `StateStore`
used by `ironcrew chat`.

| Method | Path                                                | Purpose                             |
| ------ | --------------------------------------------------- | ----------------------------------- |
| POST   | `/flows/{flow}/conversations/{id}/start`            | Create or re-open a chat session    |
| POST   | `/flows/{flow}/conversations/{id}/messages`         | Send a user turn, wait for a reply  |
| GET    | `/flows/{flow}/conversations/{id}/history`          | Read the stored transcript          |
| GET    | `/flows/{flow}/conversations/{id}/events`           | SSE stream for the session          |
| DELETE | `/flows/{flow}/conversations/{id}`                  | Drop handle + delete record         |
| GET    | `/flows/{flow}/conversations`                       | Paginated list (filtered by flow)   |

`POST /messages` against an unknown id returns `404` — sessions never
auto-create. Only one mutating operation may be active for a conversation;
an overlapping message returns `409 Conflict` so the server does not retain
an unbounded per-session request queue. Clients should retry with bounded
backoff after the current turn completes. The hard cap on simultaneously-active sessions is
`IRONCREW_MAX_ACTIVE_CONVERSATIONS` (default 8); breaches return `503`.
Text and image inputs are independently bounded; see the
[chat environment table](chat.md#environment-variables) for the exact defaults
and hard ceilings.

See [docs/chat.md](chat.md) for the full reference, request/response
shapes, and a worked curl session.

## GET /audit

Returns the audit log of state-changing API actions, sorted newest-first
with pagination.

### Query parameters

| Param | Type | Notes |
|---|---|---|
| `flow` | string | Filter by `flow_path` (exact match). |
| `action` | string | One of `flow.run.start`, `flow.run.abort`, `flow.run.delete`, `conversation.start`, `conversation.message`, `conversation.delete`. |
| `actor` | string | Exact match against the `X-Audit-Actor` value at write time. |
| `since` | RFC3339 timestamp | Inclusive lower bound. |
| `until` | RFC3339 timestamp | Exclusive upper bound. |
| `success` | `true` / `false` | Filter to only successful or only failed attempts. |
| `limit` | int | Page size. Default `IRONCREW_AUDIT_DEFAULT_LIMIT` (50). Capped at `IRONCREW_AUDIT_MAX_LIMIT` (500). |
| `offset` | int | Skip the first N rows. Default 0. |

Server-owned keyed runs and message turns retain audit ownership after a
client disconnect. `conversation.message` metadata contains only idempotency
mode and turn indexes/counts; message content and raw idempotency/recovery keys
are never copied into the audit event.

### Response

```json
{
  "events": [
    {
      "id": "f3e1c2a8-...",
      "timestamp": "2026-05-21T10:00:00Z",
      "action": "flow.run.delete",
      "flow_path": "chat-http",
      "target": "run-xyz",
      "actor": "alice@example.com",
      "source_ip": "203.0.113.7",
      "success": true,
      "status_code": 200,
      "metadata": null
    }
  ],
  "total": 1234,
  "limit": 50,
  "offset": 0
}
```

### `X-Audit-Actor` header

Every state-changing endpoint accepts an optional `X-Audit-Actor`
request header. The value is recorded into the audit event's `actor`
field. Voluntary, validated (≤256 chars, no control characters,
trimmed). Future JWT integration will override with the `sub` claim.

### Authorization

Same `IRONCREW_API_TOKEN` as the other protected endpoints. No
dedicated audit token today. Reads of the audit log are not
themselves audited.

### Trust-proxy mode

When running behind a reverse proxy (Nginx, Envoy, AWS ALB, etc.),
set `IRONCREW_TRUST_PROXY=1` so the audit recorder uses the first hop
of `X-Forwarded-For` instead of the direct TCP peer for `source_ip`.
Without the env var set, an attacker hitting the server directly could
forge their IP by sending an `X-Forwarded-For` header; the gate
prevents that.

## Health Check

```bash
curl http://localhost:3000/health
```

```json
{
  "status": "ok",
  "version": "2.24.0"
}
```

The `version` field is populated from the crate's `CARGO_PKG_VERSION`, so it
always reflects the binary you are actually running.

## Authentication

Set `IRONCREW_API_TOKEN` to require Bearer token authentication on all endpoints
except the public health routes. Public/non-loopback binds require a token by
default, along with an explicit `IRONCREW_STORE`; the server refuses to start
otherwise. A configured token must contain 32–4096 visible ASCII bytes without
spaces; an empty, Unicode, or otherwise malformed value fails startup:

```bash
IRONCREW_API_TOKEN=replace-with-a-random-32-byte-token ironcrew serve --flows-dir ./flows
```

Callers must include the token in the `Authorization` header:

```bash
curl -H "Authorization: Bearer replace-with-a-random-32-byte-token" \
  http://localhost:3000/flows/simple/run -X POST
```

| Scenario | Result |
|----------|--------|
| Token absent on a loopback bind | All requests pass (local development only) |
| Token absent on a public bind | Startup fails unless `IRONCREW_ALLOW_UNAUTHENTICATED=true` is explicitly set |
| Token set, no header | `401 {"error":"Missing Authorization header"}` |
| Token set, wrong token | `401 {"error":"Invalid token"}` |
| Token set, correct token | Request proceeds normally |
| `/health`, `/health/live`, `/health/ready` | Always public, no token needed |

Authentication priority (for future extensibility):
1. `IRONCREW_API_TOKEN` — static token, checked locally (highest priority)
2. (Future) Remote token validation service via external URL

## CORS

CORS is configured via the `IRONCREW_CORS_ORIGINS` environment variable:

| Value | Behavior |
|-------|----------|
| Absent (default) | No origins allowed (API not accessible from browsers) |
| `*` | Permissive — all origins allowed (development only) |
| Comma-separated URLs | Only listed origins allowed |

```bash
# Allow specific origins
IRONCREW_CORS_ORIGINS=https://app.example.com,https://admin.example.com

# Allow all (development only)
IRONCREW_CORS_ORIGINS=*
```

Allowed methods: GET, POST, DELETE, OPTIONS. Allowed request headers:
`Authorization`, `Content-Type`, `Idempotency-Key`, and
`Idempotency-Recovery-Key`. Browser clients may read the exposed
`Idempotency-Replayed` response header.

## Request Size Limits

The server enforces a maximum request body size (default 10 MiB). Override with
`IRONCREW_MAX_BODY_SIZE` (in bytes):

```bash
IRONCREW_MAX_BODY_SIZE=8388608  # 8 MiB
```

Values must be positive and cannot exceed 64 MiB.

## Error Responses

API error responses are sanitized to prevent leaking internal filesystem paths
or server structure. Full error details are logged server-side.

## Graceful Shutdown

The server handles `SIGTERM` and `Ctrl+C` for graceful shutdown. On receipt of
the signal, IronCrew:

1. Marks readiness unavailable, stops accepting new work, and enters Axum's
   graceful-shutdown path.
2. Aborts every active run, waits for its monitor to persist an `aborted`
   terminal record and emit the terminal event, then drops run and conversation
   event buses so SSE streams close cleanly.
3. Waits out a short post-serve drain window to let background cleanup tasks
   (e.g. MCP child-process reapers) finish.

Two environment variables tune the behavior:

| Variable                         | Default | Purpose                                                                                   |
|----------------------------------|---------|-------------------------------------------------------------------------------------------|
| `IRONCREW_SHUTDOWN_TIMEOUT_SECS` | `10`    | Hard deadline after the signal is received; the server exits even if Axum graceful shutdown has not finished yet. |
| `IRONCREW_SHUTDOWN_DRAIN_MS`     | `1000`  | Post-serve drain window for background cleanup tasks (MCP child-process reaping, etc.).   |

This is essential for Kubernetes rolling updates and Railway deployments —
SSE consumers see a clean disconnect and can reconnect to a fresh pod.

## Docker Deployment

Build and run with Docker:

```bash
docker build -t ironcrew .
docker run -p 3000:3000 \
  --env-file .env \
  -e IRONCREW_CORS_ORIGINS=https://app.example.com \
  -e IRONCREW_API_TOKEN=replace-with-a-random-32-byte-token \
  -v ./flows:/flows:ro \
  ironcrew
```

The Dockerfile uses a reproducible multi-stage build: Rust `1.96.0` with
`cargo build --release --locked`, then a `debian:13-slim` runtime with only CA
certificates. The image runs as numeric non-root UID `10001` (group `0`) and
provides `ironcrew serve --flows-dir /flows` as its default command.

The image sets `IRONCREW_HOST=0.0.0.0`, so the published port is reachable
without an extra bind flag. A host-built binary still defaults to `127.0.0.1`
unless `PORT`, `IRONCREW_HOST`, or `--host` selects another address.
