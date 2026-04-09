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

The server loads `.env` from the current working directory on startup, so API keys
set there are available to all flows.

## Endpoints

| Method   | Path                              | Description                        |
|----------|-----------------------------------|------------------------------------|
| GET      | `/health`                         | Health check (returns version)     |
| POST     | `/flows/{flow}/run`               | Start a crew run (async)           |
| POST     | `/flows/{flow}/abort/{run_id}`    | Abort a running crew               |
| GET      | `/flows/{flow}/events/{run_id}`   | SSE event stream for a run         |
| GET      | `/flows/{flow}/runs`              | List past runs for a flow          |
| GET      | `/flows/{flow}/runs/{id}`         | Get a specific run record          |
| DELETE   | `/flows/{flow}/runs/{id}`         | Delete a run record                |
| GET      | `/flows/{flow}/validate`          | Validate a flow (syntax + agents)  |
| GET      | `/flows/{flow}/agents`            | List agents defined in a flow      |
| GET      | `/nodes`                          | List all built-in tools            |

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

Each run has a maximum lifetime (default: 30 minutes). If execution exceeds this
limit, the run is aborted and a `run_complete` event is emitted with `status: "timeout"`.
Configure via `IRONCREW_MAX_RUN_LIFETIME` env var (seconds).

## Aborting a Run

Cancel a running crew by calling the abort endpoint:

```bash
curl -X POST http://localhost:3000/flows/my-crew/abort/abc-123
# {"run_id":"abc-123","status":"aborted"}
```

This immediately cancels all in-flight LLM calls and drops pending tasks.
The SSE stream receives a `run_complete` event with `status: "aborted"`.
The run is cleaned up after 5 seconds.

A run can end with one of these statuses:
- `success` / `partial_failure` / `failed` — normal completion
- `timeout` — 30-minute lifetime exceeded
- `aborted` — cancelled via this endpoint

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

## Run History

### List Runs

Paginated, metadata-only listing of past runs. The response body is an
object with `runs`, `total`, `limit`, and `offset` — **not** a bare array.
Individual run summaries omit `task_results` so listings stay cheap even on
stores with thousands of historical runs.

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
| `status` | string  | `success`, `partial_failure`, `failed` |
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
identically regardless of backend. Each flow gets its own store instance.

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

## Health Check

```bash
curl http://localhost:3000/health
```

```json
{
  "status": "ok",
  "version": "1.3.0"
}
```

## Authentication

Set `IRONCREW_API_TOKEN` to require Bearer token authentication on all endpoints
except `/health`:

```bash
IRONCREW_API_TOKEN=my-secret-token ironcrew serve --flows-dir ./flows
```

Callers must include the token in the `Authorization` header:

```bash
curl -H "Authorization: Bearer my-secret-token" \
  http://localhost:3000/flows/simple/run -X POST
```

| Scenario | Result |
|----------|--------|
| `IRONCREW_API_TOKEN` not set | All requests pass (no auth) |
| Token set, no header | `401 {"error":"Missing Authorization header"}` |
| Token set, wrong token | `401 {"error":"Invalid token"}` |
| Token set, correct token | Request proceeds normally |
| `/health` endpoint | Always public, no token needed |

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

Allowed methods: GET, POST, DELETE, OPTIONS. Allowed headers: `Authorization`, `Content-Type`.

## Request Size Limits

The server enforces a maximum request body size (default 10MB). Override with
`IRONCREW_MAX_BODY_SIZE` (in bytes):

```bash
IRONCREW_MAX_BODY_SIZE=52428800  # 50MB
```

## Error Responses

API error responses are sanitized to prevent leaking internal filesystem paths
or server structure. Full error details are logged server-side.

## Graceful Shutdown

The server handles `SIGTERM` and `Ctrl+C` for graceful shutdown. In-flight
requests are allowed to complete before the process exits. This is essential for
Kubernetes rolling updates and Railway deployments.

## Docker Deployment

Build and run with Docker:

```bash
docker build -t ironcrew .
docker run -p 3000:3000 \
  --env-file .env \
  -e IRONCREW_CORS_ORIGINS=https://app.example.com \
  -e IRONCREW_API_TOKEN=your-secret-token \
  -v ./flows:/app/flows \
  ironcrew serve --host 0.0.0.0 --port 3000 --flows-dir /app/flows
```

The Dockerfile uses a multi-stage build: Rust compilation in `rust:latest`,
then a minimal `debian:13-slim` runtime with only `ca-certificates`.

When running in Docker, bind to `0.0.0.0` (not the default `127.0.0.1`) so the
port mapping works correctly.
