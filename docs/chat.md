# Chat & Conversations (Phase 1)

Phase 1 Human-in-the-Loop turns IronCrew's existing `crew:conversation({...})`
primitive into a first-class chat runtime — you drive it interactively from
the terminal or over HTTP, with the same shared state and persistence layer.

Two surfaces share the underlying mechanism:

- `ironcrew chat <path>` — a local REPL.
- `POST /flows/{flow}/conversations/{id}/{start,messages,...}` — HTTP endpoints
  under the existing REST API.

(For the *mid-run* counterpart — a flow that pauses itself to ask the human a
question during execution — see [`crew:ask_human`](crews.md#human-in-the-loop-ask_human).)

Both paths route through the same `LuaConversationInner` state, so a chat
you start in the CLI can be continued later from the API and vice versa.
Persistent conversations backed by PostgreSQL have one additional boundary:
turns must enter through the keyed HTTP `/messages` endpoint. Direct Lua/CLI
`send()` and `ask()` calls do not acquire the shared durable turn fence and
therefore fail closed for a persistent PostgreSQL conversation.

## Canonical mode-guard pattern

IronCrew exposes `IRONCREW_MODE` as a Lua global. It is `"run"` during a
normal `ironcrew run` or API run, and `"chat"` while the CLI REPL or the
HTTP `start` handler is building a session. Write your top-level script so
the crew is always declared, but any one-shot `crew:run()` only fires in
run mode:

```lua
local crew = Crew.new({ goal = "...", provider = "openai", model = "gpt-5.6-luna" })
crew:add_agent(Agent.new({ name = "tutor", goal = "..." }))

if IRONCREW_MODE ~= "chat" then
    crew:add_task({ name = "demo", agent = "tutor", description = "..." })
    crew:run()
end
```

That way the same `crew.lua` works for both `ironcrew run` and
`ironcrew chat`.

HTTP conversation bootstrap also enforces this rule in the host. While the
entrypoint is being evaluated to discover its declarative Crew, Agent, task,
provider, and tool definitions, effectful capabilities such as `crew:run()`,
`crew:conversation()`, `crew:dialog()`, `crew:ask_human()`, `run_flow()`,
`http.*`, `fs.write()`, crew messages, and crew-memory mutation are rejected
before physical work starts. This makes cold rehydration on another replica
safe to repeat. The mode guard remains required for CLI chat, where that
HTTP-only purity marker is not installed.

An HTTP conversation build captures one bounded, UTF-8, no-follow immutable
snapshot of the flow's Lua tree. That single capture supplies the bytes for
`config.lua`, `crew.lua`, direct `agents/*.lua`, `tools/*.lua`, `_lib`
`require`, and every nested `run_flow`; the build never mixes those files with
a later filesystem read. After construction, a second bounded capture is used
only to detect an edit or rollout that crossed the build and returns `409` on
drift. It is not a second execution source. Ordinary `ironcrew run` and
`ironcrew chat` continue to load their Lua sources from the filesystem. If a
platform cannot provide the secure no-follow traversal, HTTP conversation
construction fails closed rather than falling back to a racy loader.
The capture accepts regular `.lua` files only, rejects symlinks, special files,
invalid UTF-8, and files that change while read, and applies
`IRONCREW_LUA_MAX_SOURCE_BYTES` per file (1 MiB by default, 16 MiB hard
maximum). Hard tree bounds are 1,024 Lua files, 64 MiB of Lua source, 16,384
entries, and 32 directory levels per capture.

## CLI: `ironcrew chat <path>`

```
ironcrew chat <path> [--agent <name>] [--id <conversation_id>]
```

- `<path>` — project directory or `crew.lua` file. Same semantics as
  `ironcrew run`.
- `--agent <name>` — the agent declared in your `crew.lua` to converse
  with. Required — the REPL picks no default.
- `--id <conversation_id>` — stable session id. When set, the session is
  persisted via the configured `StateStore` and is eligible for cross-run
  resume. Without `--id`, the session is ephemeral. A persistent PostgreSQL
  conversation must be driven through the HTTP API instead, because shared
  turns require an `Idempotency-Key` and a durable incarnation/revision fence.

Slash commands:

| Command           | Effect                                 |
| ----------------- | -------------------------------------- |
| `/help`, `/?`     | Show available commands                |
| `/exit`, `/quit`  | End the session                        |
| `/reset`          | Clear history (keep the system prompt) |
| `/id`             | Print the session id                   |
| `/save`           | Persist the session now                |
| `/history`        | Dump the full transcript               |

Example (full session against `examples/chat-cli`):

```sh
export OPENAI_API_KEY=sk-...
ironcrew chat examples/chat-cli --agent tutor --id onboarding-2026-04
```

See [examples/chat-cli/README.md](../examples/chat-cli/README.md) for more.

## HTTP API

All endpoints sit under `/flows/{flow}/conversations` and are protected by
the existing `IRONCREW_API_TOKEN` bearer middleware. `flow` passes through
`resolve_flow_path` and `id` through `validate_session_id`, so traversal
attempts and SQL metacharacters are rejected before they reach the store.

| Method | Path                                                | Purpose                         |
| ------ | --------------------------------------------------- | ------------------------------- |
| POST   | `/flows/{flow}/conversations/{id}/start`            | Create or re-open a session     |
| POST   | `/flows/{flow}/conversations/{id}/messages`         | Send a user turn, get a reply   |
| GET    | `/flows/{flow}/conversations/{id}/history`          | Read the persisted transcript   |
| GET    | `/flows/{flow}/conversations/{id}/events`           | SSE stream of conversation events |
| DELETE | `/flows/{flow}/conversations/{id}`                  | Drop the handle + stored record |
| GET    | `/flows/{flow}/conversations`                       | Paginated list of sessions      |

Every response produced by these registered conversation routes carries
`Cache-Control: no-store`, including successful JSON, authentication,
extractor, admission, validation, conflict, and capacity responses. A
successful local SSE response strengthens that to `no-store, no-transform`
and also sends `X-Accel-Buffering: no`.

### POST `/start`

Body:

```json
{ "agent": "tutor", "max_history": 50 }
```

- `agent` — required for a new session. A durable resume may omit it; when
  supplied, it must match both the stored conversation agent and an agent
  declared in `crew.lua`.
- `max_history` — optional per-session cap. HTTP sessions default to
  `IRONCREW_API_CONVERSATION_MAX_HISTORY` (50) and reject zero or values above
  that server policy (hard ceiling 1000).

Response:

```json
{
  "conversation_id": "onboarding",
  "flow": "chat-http",
  "agent": "tutor",
  "created_at": "2026-04-18T10:15:00Z",
  "turn_count": 0,
  "revision": 1,
  "incarnation_id": "cbe8ba31-424b-48c7-a58b-0f5cd97c0d68",
  "source_fingerprint": "sha256:...",
  "definition_fingerprint": "sha256:...",
  "events_url": "/flows/chat-http/conversations/onboarding/events"
}
```

Calling `/start` twice with the same id returns the current durable identity
and metadata. The live Lua handle is a cache and may be rebuilt when the
request reaches a process that does not have it.

**Resuming an evicted session.** If a session has a record in storage but no
live in-memory handle (for example after idle eviction), `POST /start` with
an empty `{}` body is enough to bring it back. IronCrew looks up the stored
agent and rebuilds the handle for you — clients do not need to remember the
original agent name to reactivate an evicted session. Passing an `agent`
field is still allowed and must match the stored agent. A non-empty mismatch
returns `409 Conflict` before IronCrew evaluates the flow or constructs a live
Lua conversation, and leaves the stored transcript and active-session state
unchanged. Agent matching is exact after trimming surrounding whitespace.
The stored `max_history`, source fingerprint, definition fingerprint, and
incarnation must also match. Definition drift fails with `409`; restore the
original definition or delete the old record and create a new conversation.
The definition includes the selected Agent, resolved model and system prompt,
provider/tool graph, transcript limits, and maximum tool rounds. It also binds
the effective non-secret policy captured by the constructed runtime: provider
endpoint/options and rate/response/output limits; Lua VM, JSON, HTTP,
conversation-input, reasoning, and tool-dispatch limits; approval policy;
network-private opt-in; maximum nested-flow depth; and the fingerprints of
reachable tools' capability roots. The turn executes with those captured
values rather than re-reading mutable process environment settings. Raw roots,
credentials, and secret values are not stored in the identity. Code-fixed
ceilings and semantics remain a deployment artifact-parity responsibility,
represented by the separately attested artifact fingerprint.

**503 on cap exceeded.** Returns `503 Service Unavailable` when
`IRONCREW_MAX_ACTIVE_CONVERSATIONS` is reached and the request would construct
a new live handle, whether for a new record or a cold resume. Returning an
already-current local handle needs no new permit. The permit is reserved before
Lua construction and fresh persistence, so a cap rejection leaves no orphaned
record behind.

### POST `/messages`

Body:

```json
{ "content": "Hello", "images": ["images/chart.png"] }
```

Blocks until the full turn (including any tool-call rounds) completes.
Only one mutating request may run for a `(flow, id)`. An overlapping request
returns `409 Conflict` immediately rather than being retained in a server-side
queue.

Returns:

```json
{
  "conversation_id": "onboarding",
  "turn_index": 0,
  "assistant": "Hi! ...",
  "reasoning": "(optional)",
  "turn_count": 1,
  "revision": 2,
  "incarnation_id": "cbe8ba31-424b-48c7-a58b-0f5cd97c0d68",
  "definition_fingerprint": "sha256:..."
}
```

Returns `404` if no durable conversation record exists. **`POST /messages`
never creates a session implicitly — call `/start` first.** With PostgreSQL,
the record can be cold-rehydrated on whichever replica receives the message;
the caller does not need to route through the process that handled `/start` or
a prior turn. Cold rehydration still needs a local active-conversation permit;
capacity exhaustion returns `503` without running the turn.

Send one stable `Idempotency-Key` for the logical message and reuse it only for
the exact same body. PostgreSQL-backed conversation messages require this
header even when `IRONCREW_REQUIRE_IDEMPOTENCY_KEY=false`; JSON and SQLite obey
the configured global policy. A completed retry replays the stored response
with `Idempotency-Replayed: true`; an executing, conflicting, or
crash-indeterminate request returns `409` and never launches a duplicate turn.
Set `IRONCREW_REQUIRE_IDEMPOTENCY_KEY=true` in production so run admission has
the same policy. See
[REST API safe retries](rest-api.md#safe-retries-with-idempotency-key) for the
full contract, the explicit `Idempotency-Recovery-Key` procedure after an
inspected indeterminate turn, and the external-tool boundary.

Owner-death recovery is deliberately between turns. If the prior owner died
after a turn committed, the next keyed message can rebuild from that exact
revision on another replica. If it died while provider/tool work or commit may
have been in flight, IronCrew does not take over the Lua VM or assume that
external effects did not occur: the key remains indeterminate until the client
inspects durable history and follows the recovery-key barrier.

The durable message claim is scoped to `(flow, id, incarnation_id)` and its
base revision. A cold Lua handle is constructed only after that claim is live,
and transcript plus replay response commit atomically against the same
execution identity and revision. A concurrent delete returns `409` while a
turn is active; a later delete/recreate receives a new incarnation, so an old
key or response cannot cross the ABA boundary.

JSON/SQLite conversations accept project-relative paths. PostgreSQL
conversations reject local paths because another replica cannot observe the
same process-local bytes; use public `http://` or `https://` locators instead.
Remote locators use the protected public-network client. A remote image must return a successful
status and an `image/jpeg`, `image/png`, `image/gif`, or `image/webp` content
type; its body is streamed under the byte cap. Message text defaults to a 256 KiB
cap. Image defaults are 4 locators/20 MiB decoded per message and 16
locators/32 MiB decoded per conversation, with a 2048-byte locator cap. Tune
these with the `IRONCREW_API_*` variables below; the process also applies
`IRONCREW_MAX_IMAGE_BYTES` to each loaded image.

### GET `/history`

Reads directly from the store. Works even after the in-memory handle has
been evicted:

```json
{
  "conversation_id": "onboarding",
  "flow": "chat-http",
  "agent": "tutor",
  "created_at": "...",
  "updated_at": "...",
  "messages": [
    { "role": "system",    "content": "..." },
    { "role": "user",      "content": "Hello" },
    { "role": "assistant", "content": "Hi!" }
  ],
  "turn_count": 1,
  "truncated": false,
  "revision": 2,
  "incarnation_id": "cbe8ba31-424b-48c7-a58b-0f5cd97c0d68",
  "source_fingerprint": "sha256:...",
  "definition_fingerprint": "sha256:..."
}
```

History is the recovery surface for every backend and is read directly from
durable storage. Records created before execution identities were introduced
remain readable here so clients can export their transcript, and they can be
deleted, but `/start` and `/messages` reject them. Export the history, delete
the legacy record, and start a new conversation rather than silently assigning
it a modern identity. This compatibility applies only to otherwise valid,
bounded records. All backends reject structurally amplified JSON, oversized
metadata/identity or transcripts, non-array transcripts, excessive message
counts, and invalid message shapes before adopting them as conversation state.
SQL backends apply their byte/count checks before returning the stored JSON to
Rust; JSON files apply the same bounded structural preflight to the whole
record. A corrupt row is not treated as an identity-less legacy conversation.
`DELETE` remains an operator recovery path because it identifies and removes
the row without deserializing its transcript, subject to the same active-turn
fence.

### GET `/events` (SSE)

This endpoint has an explicit backend boundary. With PostgreSQL shared-store
coordination, an existing conversation returns `409 Conflict`: conversation
SSE replay is unsupported, and durable `/history` is the recovery surface. A
missing record returns `404`. With JSON or SQLite, the endpoint subscribes to
the current process's per-session `EventBus`, replays its bounded in-memory
buffer, then tails live events. `Last-Event-ID` is not supported and returns
`409`; reconnect without a cursor. The following event types are forwarded to
local chat subscribers:

**Conversation lifecycle**

- `conversation_started`
- `conversation_turn`
- `conversation_thinking`

**Sub-crew progress (fired when a conversational tool delegates to a
sub-flow via `run_flow`, so the UI can render progress during the turn):**

- `crew_started`
- `phase_start`
- `task_assigned`
- `task_completed`
- `task_failed`
- `task_thinking`
- `tool_call`
- `tool_result`
- `agent_tool_started` — fires immediately before a sub-agent runs via
  `agent__<name>`; fields: `caller`, `callee`, `prompt`
- `agent_tool_completed` — fires when the sub-agent returns; fields:
  `caller`, `callee`, `duration_ms`, `success`

The stream also emits a `keepalive` comment every 15 seconds so proxies
don't idle the connection out. Successful streams use `Content-Type:
text/event-stream`, `Cache-Control: no-store, no-transform`, and
`X-Accel-Buffering: no`. Validation, missing-session, authentication, and
connection-cap errors are non-cacheable; a saturated process returns `429`
with numeric `Retry-After`.

If a process-local subscriber falls behind the bounded broadcast buffer, the
stream emits one `conversation_gap` event with the skipped count and a
direction to read durable history, then closes. It never silently resumes past
lost events.

**After local eviction.** With JSON or SQLite, once a session has been evicted
from memory (idle timeout or explicit `DELETE`), `GET /events` returns **`404
Not Found`**.
Clients must call `POST /start` (with an empty `{}` body to re-use the
stored agent — see above) to re-activate the session before subscribing to
events again. Re-activation does not create cursor replay for events emitted by
the old handle.

### DELETE `/{id}`

Drops the local in-memory handle (if any), then removes the persisted record.
The lifecycle gate and durable store fence both reject deletion while a keyed
message is active. A racing `DELETE` returns `409` promptly on the turn's own
replica and on a peer replica; it does not wait for or cancel the turn. Retry
after the turn reaches a durable outcome.

### GET `/conversations`

Query params:

- `limit` — defaults to `IRONCREW_CONVERSATIONS_DEFAULT_LIMIT` (20),
  capped at `IRONCREW_CONVERSATIONS_MAX_LIMIT` (100).
- `offset` — pagination cursor, default 0.

Returns paginated summaries filtered by the flow's `flow_path` — legacy
records without a `flow_path` value are invisible to all per-flow HTTP
endpoints and require a global/admin store migration. They are distinct from
flow-scoped, identity-less execution records, which remain readable through
`/history` and removable with `DELETE` as described above.

## Server-wide store singleton

Under `ironcrew serve`, the `StateStore` used for chat persistence is a
**server-wide singleton** bootstrapped once at startup in `cmd_serve`
(`src/cli/server.rs`). Postgres connection setup and migrations run exactly
once for the process, and every `/start`, `/messages`, and `/history` call
reuses the shared pool. In practice this keeps `POST /start` latency at
roughly ~10 ms instead of the ~300 ms a per-request bootstrap would cost.

## Graceful shutdown

An explicit Unix `SIGUSR1` drain keeps active chat handles and their SSE
streams observable, but a new conversation mutation whose lifecycle middleware
check occurs after withdrawal, including `/start`, `/messages`, and `DELETE`,
gets non-cacheable `503 instance_draining` and `Retry-After: 1`. Read-only
history/list operations remain available. The middleware check is the
admission linearization point: a request admitted just before withdrawal can
lose an inner race and receive a generic non-cacheable `503` with numeric
`Retry-After` instead. This prevents a replica that has withdrawn from
readiness from starting a new turn it may not have time to finish.

On `SIGTERM` or Ctrl+C, `ironcrew serve` first enters that lifecycle boundary,
commits the durable fence, waits any remainder of the bounded routing interval,
then cancels active chat turns, persists the latest safe revision, and
terminates their SSE streams (see
[rest-api.md](rest-api.md#graceful-shutdown) for the shared lifecycle and
timing knobs). Clients should treat any local SSE disconnect as expected.
After the replacement is ready:

1. PostgreSQL clients may send the next keyed `/messages` request through any
   replica; it cold-rehydrates from the durable transcript. Use `/history` for
   recovery because shared-store conversation SSE is unsupported.
2. JSON/SQLite clients call `/start` with an empty `{}` body, then reconnect to
   local `/events` without `Last-Event-ID`.

The persisted transcript survives process replacement. `/history` remains
available during explicit drain, but no HTTP endpoint is promised after the
process enters Stopping.

## Environment variables

| Variable                               | Default | Purpose                                                      |
| -------------------------------------- | ------- | ------------------------------------------------------------ |
| `IRONCREW_API_TOKEN`                   | —       | Bearer token for the protected REST API; when set, must be 32–4096 visible ASCII bytes without spaces |
| `IRONCREW_CHAT_SESSION_IDLE_SECS`      | 1800    | Idle window after which a session handle is evicted          |
| `IRONCREW_MAX_ACTIVE_CONVERSATIONS`    | 8       | Simultaneous in-memory session cap                           |
| `IRONCREW_CONVERSATIONS_DEFAULT_LIMIT` | 20      | Default page size for `GET /conversations`                   |
| `IRONCREW_CONVERSATIONS_MAX_LIMIT`     | 100     | Hard cap on `?limit=`                                        |
| `IRONCREW_CONVERSATION_MAX_HISTORY`    | 50      | Default retained messages for Lua/CLI conversations (hard ceiling 4096; zero is rejected) |
| `IRONCREW_API_CONVERSATION_MAX_HISTORY` | 50     | HTTP `max_history` default/policy cap (hard ceiling 1000)     |
| `IRONCREW_API_MESSAGE_MAX_BYTES`       | 262144  | Maximum text bytes in one HTTP message (hard ceiling 4 MiB)   |
| `IRONCREW_API_MAX_IMAGES_PER_MESSAGE`  | 4       | Image-count cap per message (hard ceiling 32)                 |
| `IRONCREW_API_MAX_IMAGES_PER_CONVERSATION` | 16  | Cumulative image-count cap (hard ceiling 256)                 |
| `IRONCREW_API_MAX_IMAGE_BYTES_PER_MESSAGE` | 20971520 | Decoded image-byte cap per message (hard ceiling 100 MiB) |
| `IRONCREW_API_MAX_IMAGE_BYTES_PER_CONVERSATION` | 33554432 | Cumulative decoded image-byte cap (hard ceiling 512 MiB) |
| `IRONCREW_API_MAX_IMAGE_LOCATOR_BYTES` | 2048    | Path/URL/data-URL locator cap (hard ceiling 16 KiB)            |
| `IRONCREW_REQUIRE_IDEMPOTENCY_KEY`      | false   | Require a valid key for run and JSON/SQLite message mutations; PostgreSQL conversation messages require a key regardless; recommended `true` in production |
| `IRONCREW_IDEMPOTENCY_TTL_SECONDS`      | 86400   | Completed/indeterminate replay retention (60–2592000; must exceed max run lifetime by one hour) |
| `IRONCREW_IDEMPOTENCY_MAX_RESPONSE_BYTES` | 8388608 | Maximum compact response retained for one key (hard ceiling 64 MiB) |

## Live curl session

```sh
export IRONCREW_API_TOKEN=dev-token-change-me-32-bytes-minimum
BASE=http://127.0.0.1:3000

curl -sX POST "$BASE/flows/chat-http/conversations/demo/start" \
     -H "Authorization: Bearer $IRONCREW_API_TOKEN" \
     -H 'Content-Type: application/json' \
     -d '{ "agent": "concierge" }'

curl -sX POST "$BASE/flows/chat-http/conversations/demo/messages" \
     -H "Authorization: Bearer $IRONCREW_API_TOKEN" \
     -H 'Idempotency-Key: demo-message-1' \
     -H 'Content-Type: application/json' \
     -d '{ "content": "Hi!" }'

# JSON/SQLite only. PostgreSQL conversation SSE returns 409; use /history.
curl -sN "$BASE/flows/chat-http/conversations/demo/events" \
     -H "Authorization: Bearer $IRONCREW_API_TOKEN" &

curl -s "$BASE/flows/chat-http/conversations/demo/history" \
     -H "Authorization: Bearer $IRONCREW_API_TOKEN"

curl -sX DELETE "$BASE/flows/chat-http/conversations/demo" \
     -H "Authorization: Bearer $IRONCREW_API_TOKEN"
```

## See also

- [examples/chat-cli/](../examples/chat-cli/) — minimal REPL example.
- [examples/chat-http/](../examples/chat-http/) — curl-driven HTTP example.
- [docs/cli.md](cli.md) — other CLI subcommands.
- [docs/rest-api.md](rest-api.md) — the rest of the REST API.
- [docs/storage.md](storage.md) — how the `StateStore` backends work.
