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

Each run has a maximum lifetime of 30 minutes. If execution exceeds this limit,
the run is aborted and a `run_complete` event is emitted with status `timeout`.

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

### Event Types

| Event                | Fields                                                        | Description                                  |
|----------------------|---------------------------------------------------------------|----------------------------------------------|
| `crew_started`       | `goal`, `agent_count`, `task_count`, `model`                  | Crew execution begins                        |
| `phase_start`        | `phase`, `tasks`                                              | A new execution phase starts                 |
| `task_assigned`      | `task`, `agent`, `phase`                                      | Task assigned to an agent                    |
| `task_completed`     | `task`, `agent`, `duration_ms`, `success`, `output`, `token_usage` | Task finished successfully              |
| `task_failed`        | `task`, `agent`, `error`, `duration_ms`                       | Task execution failed                        |
| `task_skipped`       | `task`, `reason`                                              | Task skipped (condition evaluated false)     |
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

## CORS

CORS is enabled by default using a permissive policy (`CorsLayer::permissive()`),
allowing requests from any origin. This makes it straightforward to call the API
from browser-based frontends during development.

## Docker Deployment

Build and run with Docker:

```bash
docker build -t ironcrew .
docker run -p 3000:3000 \
  --env-file .env \
  -v ./flows:/app/flows \
  ironcrew serve --host 0.0.0.0 --port 3000 --flows-dir /app/flows
```

The Dockerfile uses a multi-stage build: Rust compilation in `rust:latest`,
then a minimal `debian:13-slim` runtime with only `ca-certificates`.

When running in Docker, bind to `0.0.0.0` (not the default `127.0.0.1`) so the
port mapping works correctly.
