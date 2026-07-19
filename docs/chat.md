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

## Canonical mode-guard pattern

IronCrew exposes `IRONCREW_MODE` as a Lua global. It is `"run"` during a
normal `ironcrew run` or API run, and `"chat"` while the CLI REPL or the
HTTP `start` handler is building a session. Write your top-level script so
the crew is always declared, but any one-shot `crew:run()` only fires in
run mode:

```lua
local crew = Crew.new({ goal = "...", provider = "openai", model = "gpt-4.1-mini" })
crew:add_agent(Agent.new({ name = "tutor", goal = "..." }))

if IRONCREW_MODE ~= "chat" then
    crew:add_task({ name = "demo", agent = "tutor", description = "..." })
    crew:run()
end
```

That way the same `crew.lua` works for both `ironcrew run` and
`ironcrew chat`.

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
  resume. Without `--id`, the session is ephemeral.

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

### POST `/start`

Body:

```json
{ "agent": "tutor", "max_history": 50 }
```

- `agent` — required. Must match an agent declared in `crew.lua`.
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
  "events_url": "/flows/chat-http/conversations/onboarding/events"
}
```

Idempotent — calling `/start` twice with the same id returns the current
metadata without rebuilding the session.

**Resuming an evicted session.** If a session has a record in storage but no
live in-memory handle (for example after idle eviction), `POST /start` with
an empty `{}` body is enough to bring it back. IronCrew looks up the stored
agent and rebuilds the handle for you — clients do not need to remember the
original agent name to reactivate an evicted session. Passing an `agent`
field is still allowed and must match the stored agent.

**503 on cap exceeded.** Returns `503 Service Unavailable` when
`IRONCREW_MAX_ACTIVE_CONVERSATIONS` is reached and the caller is requesting a
*new* session (reopening an existing one is always allowed). Rejected starts
no longer leak storage: the bootstrap record is only written after the
handle is successfully inserted into the active map, so a 503 leaves no
orphaned record behind.

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
  "turn_count": 1
}
```

Returns `404` if no session is active. **`POST /messages` never creates a
session implicitly — call `/start` first.**

For retry-safe production clients, send one stable `Idempotency-Key` for the
logical message and reuse it only for the exact same body. A completed retry
replays the stored response with `Idempotency-Replayed: true`; an executing,
conflicting, or crash-indeterminate request returns `409` and never launches a
duplicate turn. Set `IRONCREW_REQUIRE_IDEMPOTENCY_KEY=true` in production. See
[REST API safe retries](rest-api.md#safe-retries-with-idempotency-key) for the
full contract, the explicit `Idempotency-Recovery-Key` procedure after an
inspected indeterminate turn, and the external-tool boundary.

Paths are project-relative; remote `https://` image URLs are also accepted and
use the protected public-network client. A remote image must return a successful
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
  "turn_count": 1
}
```

### GET `/events` (SSE)

Subscribes to the per-session `EventBus`. Replays the buffered events, then
tails live ones. The following event types are forwarded to chat subscribers:

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
don't idle the connection out.

**After eviction.** Once a session has been evicted from memory (idle
timeout or explicit `DELETE`), `GET /events` returns **`404 Not Found`**.
Clients must call `POST /start` (with an empty `{}` body to re-use the
stored agent — see above) to re-activate the session before subscribing to
events again.

### DELETE `/{id}`

Drops the in-memory handle (if any), then removes the persisted record.

### GET `/conversations`

Query params:

- `limit` — defaults to `IRONCREW_CONVERSATIONS_DEFAULT_LIMIT` (20),
  capped at `IRONCREW_CONVERSATIONS_MAX_LIMIT` (100).
- `offset` — pagination cursor, default 0.

Returns paginated summaries filtered by the flow's `flow_path` — legacy
records without a `flow_path` value are invisible to per-flow listings
but still reachable via `GET /history` by direct id.

## Server-wide store singleton

Under `ironcrew serve`, the `StateStore` used for chat persistence is a
**server-wide singleton** bootstrapped once at startup in `cmd_serve`
(`src/cli/server.rs`). Postgres connection setup and migrations run exactly
once for the process, and every `/start`, `/messages`, and `/history` call
reuses the shared pool. In practice this keeps `POST /start` latency at
roughly ~10 ms instead of the ~300 ms a per-request bootstrap would cost.

## Graceful shutdown

On `SIGTERM` or `Ctrl+C`, `ironcrew serve` actively drops every active chat
session and terminates its SSE stream (see [rest-api.md](rest-api.md#graceful-shutdown)
for the shared shutdown knobs). Clients should treat the SSE disconnect as
expected and reconnect with:

1. `POST /flows/{flow}/conversations/{id}/start` with an empty `{}` body
   (re-uses the stored agent).
2. `GET /flows/{flow}/conversations/{id}/events` to resubscribe.

The persisted transcript is unaffected — `/history` continues to work
throughout.

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
| `IRONCREW_REQUIRE_IDEMPOTENCY_KEY`      | false   | Require a valid key for run and message mutations; recommended `true` in production |
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
