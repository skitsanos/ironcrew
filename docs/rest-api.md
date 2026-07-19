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
scaling considerations, see [HTTP Scaling](http-scaling.md).

## Endpoints

| Method   | Path                                               | Description                                         |
|----------|----------------------------------------------------|-----------------------------------------------------|
| GET      | `/health`                                          | Health check (returns version)                      |
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
  "events_url": "/flows/research-crew/events/a1b2c3d4-..."
}
```

The `run_id` is consistent across the initial response, SSE events, and the
persisted run record. Use the `events_url` to subscribe to real-time progress.

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

This immediately cancels all in-flight LLM calls and drops pending tasks.
The SSE stream receives a `run_complete` event with `status: "aborted"`.
Pending human-input questions expire before the abort response returns. The
run's event bus remains available for late SSE recovery and is cleaned up after
5 seconds.

A run can end with one of these statuses:
- `success` / `partial_failure` / `failed` — normal completion
- `timeout` — 30-minute lifetime exceeded
- `aborted` — cancelled via this endpoint

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

`status` is `"running"` with an empty array when nothing is pending. `404`
when the run is not active under this flow — like `abort`, the endpoint is
flow-scoped and never confirms that a run exists under a different flow.

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
# {"run_id":"abc-123","question_id":"9c1e…","status":"delivered"}
```

`answer` may be any JSON value — a string, number, or a whole object; the
suspended flow receives it as a Lua value. First writer wins: a second answer
to the same question gets `404` (the question is gone), never a silent
overwrite. `choices` are advisory — the endpoint accepts free-form answers
even when choices were offered; flows that need strict values validate in Lua.
An answer larger than `IRONCREW_ASK_HUMAN_MAX_ANSWER_BYTES` (65,536 serialized
bytes by default) gets `413 Payload Too Large` and leaves the question pending
so the client can retry with a smaller value.

Both endpoints are recorded in the [audit log](#get-audit) (actions
`flow.run.questions_list` / `flow.run.question_answer`). The bridge-generated
audit records and `human_input_*` SSE events carry the `question_id` but never
the answer content. This does not sanitize arbitrary flow logs, model output,
or tool output: flows and prompts must not echo sensitive answers themselves.

Aborting a suspended run expires its pending questions before the abort
response returns. Both question endpoints then return `404`; the separate SSE
event bus remains available for terminal-event replay during its retention
window.

## SSE Event Stream

`GET /flows/{flow}/events/{run_id}` returns a Server-Sent Events stream.

```bash
curl -N http://localhost:3000/flows/research-crew/events/a1b2c3d4-...
```

### Replay Buffer

Late subscribers receive all past events before switching to the live stream.
The replay buffer holds up to 1000 events. If a run has already completed by the
time you connect, you receive the full history (including `run_complete`) and the
stream closes immediately.

### Output Truncation

By default, SSE events include the full task output. For flows that produce
large outputs (e.g., VTT transcripts), set `IRONCREW_SSE_OUTPUT_MAX_CHARS`
to cap the output field in `task_completed` and `collaboration_turn` events:

```bash
IRONCREW_SSE_OUTPUT_MAX_CHARS=500 ironcrew serve --flows-dir ./flows
```

When truncated, the output ends with `... [truncated, N total chars]`.
Run history and the `/flows/{flow}/runs/{id}` endpoint always return the
full untruncated output.

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
| `human_input_requested` | `question_id`, `prompt`, `choices`, `timeout_s`, `kind`    | The run suspended on a human question (`kind: "question"` from ask_human, `"approval"` from a tool approval gate) — render a form / allow-deny buttons and POST to the [answer endpoint](#answer-a-question) |
| `human_input_received` | `question_id`, `outcome`                                    | The question resolved (`outcome`: `"answered"` or `"timeout"`). Never carries the answer content — answers may contain secrets |
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

A `warning` event may be sent if the subscriber falls behind and events are
dropped from the broadcast channel.

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
- SSE replay buffering is bounded separately by `IRONCREW_MAX_EVENTS` and
  `IRONCREW_EVENT_REPLAY_MAX_BYTES` (default 4 MB; see `src/engine/eventbus.rs`)

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
  "version": "2.13.0"
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
