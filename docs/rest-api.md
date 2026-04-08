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

### Modes Without SSE Events

The following Lua primitives execute inside `crew:run()` but **do not emit
SSE events** in this release:

- **`crew:conversation({})`** — single-agent multi-turn chat. Output goes to
  stderr only (with dim styling for reasoning).
- **`crew:dialog({})`** — agent-to-agent dialog. Output goes to stderr with
  `[agent_name]` prefixes per turn.

If a `crew.lua` script uses these primitives instead of (or in addition to)
tasks, REST API subscribers will only see the surrounding task events. Full
SSE wiring for conversations and dialogs (`conversation_message`,
`conversation_thinking`, `dialog_turn`, `dialog_thinking` events) is planned
for a future release.

## Run History

### List Runs

```bash
# All runs
curl http://localhost:3000/flows/research-crew/runs

# Filter by status
curl http://localhost:3000/flows/research-crew/runs?status=success
```

Valid status values: `success`, `partial_failure`, `failed`.

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
