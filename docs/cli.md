# CLI Reference

IronCrew provides a single binary, `ironcrew`, with subcommands for scaffolding,
running, validating, inspecting, and serving crew workflows.

## Global Flags

| Flag          | Description |
|---------------|-------------|
| `-v, --verbose` | Enable debug-level log output (overrides `IRONCREW_LOG`) |
| `--version`   | Print version and exit |
| `-h, --help`  | Print help for the command |

---

## Commands

### init

Scaffold a new project directory with a starter crew, sample agent, `.env`
template, and `.gitignore`.

```
ironcrew init my-project
cd my-project
```

Creates a directory with `.env`, `.gitignore`, `agents/assistant.lua`,
`tools/` (empty), and `crew.lua`.

### run

Execute a crew from a project directory or a single Lua file.

```
ironcrew run .
ironcrew run path/to/project
ironcrew run standalone.lua
ironcrew run . --input '{"topic": "Rust", "max_length": 500}'
ironcrew run . --json
ironcrew run . --input '{"topic": "Rust"}' --json 2>/dev/null | jq '.status'
```

| Flag | Description |
|------|-------------|
| `--input <JSON>` | Pass JSON data as the `input` global in Lua |
| `--json` | Output structured JSON run record instead of Lua print() |
| `--tag <label>` | Tag this run (repeatable: `--tag v2 --tag experiment`) |

- **Default path:** `.` (current directory)
- Loads `.env` (CWD first, then project dir), discovers `agents/*.lua`,
`tools/*.lua`, and `crew.lua`. Shared modules under `_lib/` are resolved on
demand via `require()`. Run history is saved to `.ironcrew/runs/`.
- In `--json` mode, Lua `print()` calls are suppressed and the full run record
(status, tasks, token usage) is written to stdout as JSON. Tracing logs go to
stderr, so piping works cleanly.
- The Lua VM has `IRONCREW_MODE = "run"` set before `crew.lua` executes
  (mirroring the `"chat"` value used by `ironcrew chat`), so scripts can
  branch on mode if they need to.

### chat

Start an interactive REPL against a conversational agent.

```
ironcrew chat .
ironcrew chat examples/chat-cli --agent tutor
ironcrew chat examples/chat-cli --agent tutor --id onboarding-2026-04
```

| Flag | Description |
|------|-------------|
| `--agent <name>` | Agent to converse with (must be declared in `crew.lua`) |
| `--id <id>` | Stable session id — enables cross-run persistence |

- The Lua VM has `IRONCREW_MODE = "chat"` set before `crew.lua` executes,
  so guard any top-level one-shot `crew:run()` with
  `if IRONCREW_MODE ~= "chat" then ... end`.
- Slash commands: `/help`, `/exit`, `/quit`, `/reset`, `/id`, `/save`,
  `/history`.
- See [docs/chat.md](chat.md) for the full reference and
  [examples/chat-cli/](../examples/chat-cli/) for a runnable example.

### validate

Check project structure and Lua syntax without executing anything.

```
ironcrew validate .
ironcrew validate path/to/project
```

Validates agent/tool file syntax, entrypoint Lua syntax, and reference
integrity (agent tool arrays vs. known tools).

### list

Display discovered agents, custom tools, built-in tools, and the entrypoint.

```
ironcrew list .
```

### nodes

List all 10 built-in tools with their descriptions.

```
ironcrew nodes
```

### serve

Start an HTTP REST API server that exposes crew workflows as endpoints.

```
ironcrew serve
ironcrew serve --host 0.0.0.0 --port 8080 --flows-dir ./flows
```

| Flag           | Default       | Description |
|----------------|---------------|-------------|
| `--host`       | environment-dependent | Bind address. Precedence: flag, `IRONCREW_HOST`, `0.0.0.0` when platform `PORT` is set, otherwise `127.0.0.1` |
| `--port`       | environment-dependent | Bind port. Precedence: flag, `IRONCREW_PORT`, platform `PORT`, then `3000` |
| `--flows-dir`  | `.`           | Directory containing crew flow subdirectories |

The `PORT` fallback works natively on Railway; no shell expansion in a start
command is required. Explicit flags always win. Invalid `IRONCREW_PORT`/`PORT`
values, an empty resolved host, and port `0` fail startup with a validation
error instead of silently using a different address.

A non-loopback/public bind also fails startup unless `IRONCREW_STORE` is set
explicitly and either `IRONCREW_API_TOKEN` or `IRONCREW_API_TOKENS` is
configured, or the unsafe `IRONCREW_ALLOW_UNAUTHENTICATED=true` override is
deliberately enabled. Local
`127.0.0.1`/`localhost` development keeps the implicit JSON/no-auth defaults.

**Endpoints:**

| Method | Path                             | Description |
|--------|----------------------------------|-------------|
| GET    | `/health`                        | Backwards-compatible lightweight liveness check |
| GET    | `/health/live`                   | Process liveness check |
| GET    | `/health/ready`                  | Lifecycle-, storage-, and lease-maintenance-aware readiness (`503` unless the process is accepting and dependencies are healthy) |
| GET    | `/capabilities`                  | Protected runtime/process identity, optional deployment evidence, and `lifecycle_state` contract |
| GET    | `/metrics`                       | Protected Prometheus process, execution, and storage metrics |
| POST   | `/flows/{flow}/run`              | Run a crew (async, returns run_id) |
| GET    | `/flows/{flow}/events/{run_id}`  | SSE event stream for a run |
| GET    | `/flows/{flow}/runs`             | List past runs for a flow |
| GET    | `/flows/{flow}/runs/{id}`        | Get run details |
| DELETE | `/flows/{flow}/runs/{id}`        | Delete a run record |
| GET    | `/flows/{flow}/validate`         | Validate a flow |
| GET    | `/flows/{flow}/agents`           | List agents in a flow |
| POST   | `/flows/{flow}/conversations/{id}/start`    | Create or re-open a chat session |
| POST   | `/flows/{flow}/conversations/{id}/messages` | Send a user turn, wait for reply |
| GET    | `/flows/{flow}/conversations/{id}/history`  | Read the stored transcript |
| GET    | `/flows/{flow}/conversations/{id}/events`   | SSE stream for the session |
| DELETE | `/flows/{flow}/conversations/{id}`          | Drop handle + delete record |
| GET    | `/flows/{flow}/conversations`               | Paginated list (filtered by flow) |
| GET    | `/nodes`                         | List built-in tools |

`/health/ready` is pessimistic about run-lease maintenance. A bounded startup
reconciliation or PostgreSQL idempotency-prune failure allows the HTTP process
to start, but readiness returns `503` with
`component: "storage_maintenance"`. A periodic owner-heartbeat or abandoned-run
reconciliation failure has the same effect. Readiness recovers only after one
complete maintenance cycle succeeds; `/health/live` and the compatibility
`/health` endpoint remain liveness-only checks.

`serve` starts in `accepting`. On Unix, `SIGUSR1` moves it through `fencing`
to `draining` without exiting: readiness fails, owned PostgreSQL keyed attempts
are fenced, protected `POST`/`DELETE` requests whose lifecycle middleware check
occurs after withdrawal reject with `503 instance_draining`, and authenticated
reads/SSE remain available. A request admitted just before withdrawal can lose
an inner race and return a generic non-cacheable `503` with numeric
`Retry-After`; the middleware phase read is the mutation-admission
linearization point.
If the explicit store fence fails, the process remains `fencing` and another
`SIGUSR1` retries. `SIGTERM` and Ctrl+C start
`IRONCREW_SHUTDOWN_ROUTING_GRACE_SECS`, retry the fence until it commits, then
enter `stopping` and bounded teardown. Railway and Kubernetes must continue
sending `SIGTERM`; configuring `SIGUSR1` as the container stop signal would
drain but never request exit.

**Deployment Evidence:**

Authenticated `GET /capabilities` always exposes `process_start_id`, a random
UUID created once for the current process start. It changes after a restart
even if the platform reuses `IRONCREW_INSTANCE_ID` or `RAILWAY_REPLICA_ID`, and
must not be used as a durable owner or routing key.

The optional deployment tuple is configured with exactly these variables:

| Variable | Capability field | Description |
|---|---|---|
| `IRONCREW_DEPLOYMENT_REVISION` | `deployment.revision` | Operator-selected immutable source/build-input revision, 1–128 bytes using ASCII letters, digits, `.`, `-`, `_`, `:`, or `+` |
| `IRONCREW_ARTIFACT_FINGERPRINT` | `deployment.artifact_fingerprint` | SHA-256 of the exact running executable/artifact |
| `IRONCREW_FLOW_FINGERPRINT` | `deployment.flow_fingerprint` | SHA-256 of the operator's documented canonical flow-tree manifest |
| `IRONCREW_CONFIG_FINGERPRINT` | `deployment.config_fingerprint` | SHA-256 of a documented canonical manifest of effective non-secret parity settings |
| `IRONCREW_HITL_KEYRING_FINGERPRINT` | `deployment.hitl_keyring_fingerprint` | SHA-256 of a canonical non-secret keyring-revision manifest |

All five absent leaves `deployment: null`; setting only a subset fails startup.
Every fingerprint is exactly `sha256:` plus 64 lowercase hexadecimal
characters. These are operator attestations, not hashes calculated by
IronCrew. Platform gates must independently recalculate them inside every
active process and correlate each response with `X-IronCrew-Instance-Id` and
`process_start_id`.

Do not put raw bearer/database/provider credentials, raw HITL keys, or hashes
of guessable secrets in either canonical manifest. A HITL revision manifest can
use key ids, the active id, and fingerprints of random 32-byte key material.
Exclude unique instance/process/platform ids, injected ports/addresses,
timestamps, and pod-specific paths from equality checks; record them as
attribution. Record platform CPU/memory limits separately. Planned key-rotation
revisions may differ temporarily only according to the ordered compatible
rollout described in [Cloud Deployment](cloud-deployment.md#hitl-key-rotation-on-railway-and-openshift).

### fmt

Lint and check Lua crew files for common issues without executing anything.

```
ironcrew fmt
ironcrew fmt path/to/project
```

Performs static analysis on the project:

| Check | Description |
|-------|-------------|
| Syntax | Parses `crew.lua`, `agents/*.lua`, and `tools/*.lua` for Lua syntax errors |
| Agent summary | Lists agents with their capabilities and tool references |
| Tool summary | Lists custom tools alongside the 10 built-in tools |
| Unknown tools | Warns when an agent references a tool that is neither built-in nor in `tools/` |

Since tasks are defined programmatically in `crew.lua` (via `crew:add_task()`),
they cannot be statically extracted. The fmt command checks `crew.lua` syntax
only and reports what it can verify without execution.

### export

Package a flow as a standalone directory for sharing. Copies the entrypoint,
agents, and tools into a clean output directory. Secrets are never copied;
instead a `.env.template` is generated with placeholder values.

```
ironcrew export .
ironcrew export path/to/project
ironcrew export . -o my-flow-export
```

| Flag           | Default                      | Description |
|----------------|------------------------------|-------------|
| `-o, --output` | `<project-name>-export`      | Output directory path |

**Included files:**
- `crew.lua` (entrypoint)
- `agents/*.lua` (all agent definitions)
- `tools/*.lua` (all custom tools)
- `.env.template` (sanitized copy of `.env` with values replaced by `<YOUR_VALUE_HERE>`)
- `.gitignore`

**Excluded (never copied):**
- `.env` (contains secrets)
- `.ironcrew/` (run history and memory)
- `output/` (generated files)

After exporting, recipients can get started with:

```
cd my-project-export
cp .env.template .env
# Edit .env with API keys
ironcrew run .
```

### graph

Generate an interactive DAG visualization of a crew project. Outputs a
self-contained HTML file that renders the crew's agents, tasks, tools,
and dependencies as a radial graph with simulation.

```
ironcrew graph .
ironcrew graph examples/research-crew
ironcrew graph . -o my-dag.html
```

| Flag           | Default                | Description |
|----------------|------------------------|-------------|
| `-o, --output` | `<project>/graph.html` | Output HTML file path |

The generated HTML requires internet for the X6 library (CDN) and IBM
Plex Sans font on first load. All crew data, JS logic, CSS, and SVG
icons are embedded inline — no other external dependencies.

Open the file in a browser to:
- View the crew structure (agents, tasks, tools, dependencies)
- Click nodes to inspect details in the right panel
- Hover nodes to highlight connected edges and neighbors
- Run a simulated execution with animated edges and status indicators

### doctor

Diagnose project health: check environment variables, project structure,
Lua syntax, and run history at a glance.

```
ironcrew doctor
ironcrew doctor path/to/project
```

Checks performed:

| Category | Details |
|----------|---------|
| Environment | `OPENAI_API_KEY` (required), `OPENAI_BASE_URL`, `OPENAI_MODEL`, `GEMINI_API_KEY`, `GROQ_API_KEY`, `ANTHROPIC_API_KEY` |
| IronCrew config | `IRONCREW_LOG`, `IRONCREW_ALLOW_SHELL`, `IRONCREW_RATE_LIMIT_MS`, `IRONCREW_MAX_RUN_LIFETIME`, `IRONCREW_STORE`, `IRONCREW_STORE_PATH` |
| Project | `.env` presence, `crew.lua` existence and syntax, `agents/` count, `tools/` count |
| Run history | Number of past runs in `.ironcrew/runs/` |

API keys are masked in output (only the first 8 characters are shown).

### runs

List past run history for a project. Output is paginated so very large run
histories don't blow up memory or the terminal.

```
ironcrew runs -p .
ironcrew runs -p . --status success
ironcrew runs -p . --tag prod --limit 50
ironcrew runs -p . --since 2026-03-01T00:00:00Z
ironcrew runs -p . --limit 20 --offset 40   # page 3
```

| Flag           | Default | Description |
|----------------|---------|-------------|
| `-p, --project`| `.`     | Project path (locates `.ironcrew/runs/`) |
| `-s, --status` | (all)   | Filter by status: `success`, `partial_failure`, `failed`, `aborted`, `timed_out`, `running`, `waiting_for_input`, `abandoned` |
| `-t, --tag`    | (all)   | Filter by tag (exact match against the run's tag list) |
| `--since`      | (all)   | Only include runs started at or after this RFC3339 timestamp |
| `-l, --limit`  | `20`    | Maximum number of runs to return |
| `-o, --offset` | `0`     | Skip the first N runs (use to page through older results) |

The listing uses a metadata-only summary view, so listing runs never pays to
load per-task outputs from disk/DB. Fetch the full record with
`ironcrew inspect <run_id>` when you need the task results.

### inspect

Show detailed information about a specific past run, including task-by-task
results, token counts, and timing.

```
ironcrew inspect <run_id> -p path/to/project
```

| Flag           | Default | Description |
|----------------|---------|-------------|
| `-p, --project`| `.`     | Project path |

### clean

Remove old run history files from `.ironcrew/runs/`.

```
ironcrew clean -p .
ironcrew clean -p . --keep 5
ironcrew clean -p . --all
```

| Flag           | Default | Description |
|----------------|---------|-------------|
| `-p, --project`| `.`     | Project path |
| `-k, --keep`  | `10`    | Keep the N most recent runs, delete the rest |
| `--all`        | `false` | Delete all runs and the memory store |

When `--all` is used, the persistent memory file (`.ironcrew/memory.json`) is
also deleted.

---

## Environment Variables

IronCrew reads environment variables for LLM provider configuration. These can
be set in the shell or in `.env` files.

**Provider & Runtime:**

| Variable          | Description |
|-------------------|-------------|
| `OPENAI_API_KEY`  | Default API key for the OpenAI-compatible provider |
| `OPENAI_BASE_URL` | Default base URL (e.g., `https://api.openai.com/v1`) |
| `OPENAI_MODEL`    | Default model name (used in `.env` templates) |
| `ANTHROPIC_API_KEY` | Rust-side default for native `provider = "anthropic"` when no custom URL is supplied |
| `GEMINI_API_KEY`  | Use explicitly as `api_key` beside a Gemini custom URL; Lua access requires `IRONCREW_ENV_ALLOWLIST` |
| `GROQ_API_KEY`    | Use explicitly as `api_key` beside a Groq custom URL; Lua access requires `IRONCREW_ENV_ALLOWLIST` |
| `MOONSHOT_API_KEY` | Use explicitly as `api_key` beside a Moonshot/Kimi custom URL |
| `DEEPSEEK_API_KEY` | Use explicitly as `api_key` beside a DeepSeek custom URL |
| `XAI_API_KEY`     | Use explicitly as `api_key` beside an xAI/Grok custom URL |
| `OPENROUTER_API_KEY` | Use explicitly as `api_key` beside an OpenRouter custom URL |
| `IRONCREW_LOG`    | Log level filter (e.g., `info`, `debug`, `trace`, `warn`, `error`) |
| `IRONCREW_ALLOW_SHELL` | Set to `1` or `true` to enable the shell tool (disabled by default) |
| `IRONCREW_RATE_LIMIT_MS` | Minimum milliseconds between LLM API calls made by one provider instance in one live Lua VM (e.g., `200` for 5 req/sec). It is not a process- or cluster-wide provider limit |
| `IRONCREW_TOOL_TIMEOUT` | Max seconds a single tool execution may run (default: `60`; hard ceiling: `3600`). Missing, invalid, or zero values use 60; larger values clamp to 3600 |
| `IRONCREW_DEFAULT_MAX_CONCURRENT` | Default max parallel tasks per phase when not set in crew config (default: `4`) |
| `IRONCREW_MAX_CONCURRENT_TASKS` | Process policy ceiling for a crew's `max_concurrent` value (default: `32`). Crew startup fails when its configured/default concurrency is outside `1..=ceiling` |
| `IRONCREW_MAX_AGENTS` | Maximum agents registered in one crew (default: `64`, hard ceiling: `1024`) |
| `IRONCREW_MAX_TASKS` | Maximum tasks registered in one crew (default: `256`, hard ceiling: `10000`) |
| `IRONCREW_CREW_GOAL_MAX_BYTES` | Maximum UTF-8 bytes in the non-empty crew goal (default: `65536`; hard ceiling: `1048576`) |
| `IRONCREW_MAX_APPROVAL_PATTERNS` | Maximum `require_approval` entries on one crew (default: `128`; hard ceiling: `1024`). Each non-empty pattern is independently capped at 512 bytes |
| `IRONCREW_MAX_MEMORY_ITEMS` | Policy ceiling for a crew's `max_memory_items` (default: `10000`; hard ceiling: `100000`; the crew setting itself defaults to `500`) |
| `IRONCREW_MAX_MEMORY_TOKENS` | Policy ceiling for a crew's `max_memory_tokens` (default: `1000000`; hard ceiling: `10000000`; the crew setting itself defaults to `50000`) |
| `IRONCREW_MAX_SERVER_TOOLS` | Maximum provider-hosted tools configured for one crew (default: `16`; hard ceiling: `64`) |
| `IRONCREW_MAX_VECTOR_STORE_IDS` | Maximum OpenAI Responses vector-store IDs configured for one crew (default: `32`; hard ceiling: `256`) |
| `IRONCREW_MAX_MODEL_ROUTES` | Maximum purpose-to-model entries in a crew's `models` table (default: `64`; hard ceiling: `256`) |
| `IRONCREW_MAX_PROMPT_CHARS` | Max user prompt size in Unicode characters (default: `102400`). Truncates with warning |
| `IRONCREW_MAX_TASK_RETRIES` | Maximum accepted per-task `max_retries` (default: `10`) |
| `IRONCREW_MAX_RETRY_BACKOFF_SECS` | Maximum accepted retry backoff and cap for exponential retry sleep (default: `300`) |
| `IRONCREW_MAX_TASK_TIMEOUT_SECS` | Maximum accepted per-task `timeout_secs` (default: `86400`) |
| `IRONCREW_MAX_COLLABORATIVE_TURNS` | Maximum accepted `max_turns` on collaborative tasks (default: `100`) |
| `IRONCREW_FOREACH_MAX_ITEMS` | Maximum array items expanded by one `foreach` task (default: `100`) |
| `IRONCREW_FOREACH_MAX_OUTPUT_BYTES` | Maximum serialized aggregate output from one `foreach` task (default: `8388608` = 8 MiB) |
| `IRONCREW_TASK_RESULT_MAX_OUTPUT_BYTES` | Maximum output bytes retained for one completed task (default: `8388608` = 8 MiB; hard ceiling: `33554432` = 32 MiB). The run fails instead of retaining an oversized result |
| `IRONCREW_TASK_RESULT_MAX_REASONING_BYTES` | Maximum reasoning bytes retained for one completed task (default: `4194304` = 4 MiB; hard ceiling: `16777216` = 16 MiB). The run fails instead of retaining an oversized result |
| `IRONCREW_RUN_RESULTS_MAX_BYTES` | Maximum aggregate serialized bytes retained in the run's task-result map (default: `33554432` = 32 MiB; hard ceiling: `50331648` = 48 MiB). Replacing a task result accounts for the replacement rather than double-counting it |
| `IRONCREW_MAX_EVENTS` | Per-run event count retained by the local replay buffer and PostgreSQL journal (default: `1000`; range: 1–10000). Oldest PostgreSQL rows are evicted with an explicit replay gap when this bound is reached. |
| `IRONCREW_EVENT_REPLAY_MAX_BYTES` | Per-run logical byte budget for the local replay buffer and PostgreSQL journal (default: `4194304` = 4 MiB; range: 1024–67108864). PostgreSQL accounts at least 1024 bytes per event; this is not a physical database-size limit. |
| `IRONCREW_EVENT_MAX_BYTES` | Maximum serialized size of one live or durable event (default: `262144` = 256 KiB; range: 1024–16777216). Must not exceed the per-run or journal-page byte limit. Oversized fields are truncated; an event that still cannot fit becomes a warning event. |
| `IRONCREW_EVENT_CHANNEL_CAPACITY` | Maximum live broadcast-ring entries per EventBus (default: `32`; hard ceiling: `256`). It is reduced automatically when needed so maximum-sized live events fit the replay byte budget |
| `IRONCREW_EVENT_JOURNAL_RETENTION_SECS` | PostgreSQL run-event logical retention from append time (default: `3600`; range: 60–2592000 seconds). Reads hide expired rows immediately; bounded physical pruning follows on append/reconciliation. |
| `IRONCREW_EVENT_JOURNAL_MAX_TOTAL_EVENTS` | Global logical retained-event budget across PostgreSQL run journals (default: `100000`; range: 1–10000000). Must be at least `IRONCREW_MAX_EVENTS`. |
| `IRONCREW_EVENT_JOURNAL_MAX_TOTAL_BYTES` | Global PostgreSQL journal logical-byte budget (default: `268435456` = 256 MiB; range: 1024–8589934592 = 8 GiB). Must be at least the per-run byte budget. Excludes indexes, row/JSONB overhead, WAL, dead tuples, and replication/backups. |
| `IRONCREW_EVENT_JOURNAL_PAGE_MAX_BYTES` | Maximum logical bytes materialized by one PostgreSQL SSE page (default: the larger of `524288` and `IRONCREW_EVENT_MAX_BYTES`; range: 1024–67108864). Must be at least the single-event limit. Page count is derived as `min(IRONCREW_MAX_EVENTS, 64)`. |
| `IRONCREW_EVENT_JOURNAL_POLL_INTERVAL_MS` | PostgreSQL SSE journal polling interval while waiting for another event (default: `500`; range: 100–5000 ms). Applies per open run stream. |
| `IRONCREW_EVENT_JOURNAL_READ_TIMEOUT_MS` | Deadline for each PostgreSQL journal-page read (default: `2000`; range: 100–30000 ms). Five consecutive read failures/timeouts close the stream with an SSE error event. |
| `IRONCREW_EVENT_JOURNAL_WRITE_TIMEOUT_MS` | Outer deadline `W` for one PostgreSQL journal-append attempt, including pool acquisition and the complete transaction (default: `1500`; range: 100–5000 ms). PostgreSQL `lock_timeout` and `statement_timeout` are `4W/5`. The writer makes at most three attempts with 50/100 ms backoffs; flush/terminal acknowledgement is bounded by `3W + 650 ms`. |
| `IRONCREW_EVENT_JOURNAL_PRUNE_BATCH` | Rows selected per bounded PostgreSQL journal-prune step (default: `1000`; range: 1–10000). Must not exceed the global event cap. |
| `IRONCREW_MESSAGEBUS_QUEUE_DEPTH` | Max messages per agent queue in the MessageBus (default: `1000`). Oldest dropped on overflow with a warning log. `0` disables the cap |
| `IRONCREW_MESSAGEBUS_PENDING_CAP` | Max pending broadcasts (messages sent before any agent is registered) (default: `500`). `0` disables the cap |
| `IRONCREW_MESSAGEBUS_MESSAGE_MAX_BYTES` | Maximum content bytes in one inter-agent message (default: `65536`); excess content is UTF-8-safely truncated |
| `IRONCREW_MESSAGEBUS_QUEUE_MAX_BYTES` | Maximum retained bytes in each agent queue (default: `4194304` = 4 MiB) |
| `IRONCREW_MESSAGEBUS_HISTORY_DEPTH` | Maximum message count in diagnostic history (default: `500`) |
| `IRONCREW_MESSAGEBUS_HISTORY_MAX_BYTES` | Maximum retained diagnostic history bytes (default: `4194304` = 4 MiB) |
| `IRONCREW_MESSAGEBUS_PENDING_MAX_BYTES` | Maximum bytes retained for broadcasts sent before agents register (default: `4194304` = 4 MiB) |

**Lua runtime:**

| Variable | Description |
|---|---|
| `IRONCREW_LUA_MAX_MEMORY_BYTES` | Per-VM Lua allocator limit (default: `33554432` = 32 MiB; range: 1 MiB–512 MiB) |
| `IRONCREW_LUA_MAX_INSTRUCTIONS` | Instruction budget reset for each top-level Lua execution (default: `50000000`; range: 100000–10000000000) |
| `IRONCREW_LUA_MAX_EXECUTION_SECONDS` | Wall-clock budget reset for each top-level Lua execution (default: `1800`; range: 1–86400) |
| `IRONCREW_LUA_MAX_SOURCE_BYTES` | Maximum bytes in any loaded `crew.lua`, agent, tool, shared module, hook, or sub-flow source (default: `1048576`; range: 1–16777216) |
| `IRONCREW_LUA_JSON_MAX_DEPTH` | Maximum nesting depth while converting Lua/JSON values (default: `64`; hard ceiling: `256`) |
| `IRONCREW_LUA_JSON_MAX_NODES` | Maximum tables/values visited in one Lua/JSON conversion (default: `100000`; hard ceiling: `1000000`) |
| `IRONCREW_LUA_JSON_MAX_STRING_BYTES` | Maximum aggregate string bytes visited in one conversion (default: `8388608`; hard ceiling: `268435456`) |
| `IRONCREW_LUA_JSON_MAX_OUTPUT_BYTES` | Maximum JSON bytes emitted by a Lua conversion/stringify (default: `16777216`; hard ceiling: `268435456`) |
| `IRONCREW_LUA_FS_MAX_READ_BYTES` | Maximum bytes read by one custom-tool `fs.read` call (default: `1048576`; range: 1–16777216) |
| `IRONCREW_LUA_FS_MAX_WRITE_BYTES` | Maximum bytes written by one custom-tool `fs.write` call (default: `1048576`; range: 1–16777216) |

**Conversations / Chat:**

| Variable          | Description |
|-------------------|-------------|
| `IRONCREW_MAX_ACTIVE_CONVERSATIONS` | Hard cap on simultaneously-active in-memory chat handles across the server (default: `8`). Breaches return `503`. Total persisted sessions are unbounded — only live handles are capped |
| `IRONCREW_MAX_CONVERSATION_LIFECYCLES` | Hard cap on distinct conversation IDs with an in-flight start/message/delete/eviction operation (default: `256`; hard ceiling: `4096`). Saturation for a new ID returns `503`; entries are removed after their final owner exits |
| `IRONCREW_MAX_ACTIVE_RUNS` | Hard cap on simultaneously in-flight flow runs (`POST /flows/{flow}/run`, default: `4`). Breaches return `503` |
| `IRONCREW_COLLABORATION_MAX_TRANSCRIPT_BYTES` | Aggregate retained collaborative-task transcript (default: `8388608` = 8 MiB; hard ceiling: 32 MiB) |
| `IRONCREW_COLLABORATION_MAX_TURN_BYTES` | Maximum provider response retained for one collaborative turn (default: `1048576` = 1 MiB; hard ceiling: 8 MiB) |
| `IRONCREW_COLLABORATION_MAX_PARTICIPANT_TURNS` | Maximum `participants × max_turns` per collaborative task (default: `64`; hard ceiling: `512`) |
| `IRONCREW_CHAT_SESSION_IDLE_SECS` | Idle timeout in seconds before an in-memory chat handle is evicted from RAM (default: `1800` = 30 min). The on-disk record stays untouched |
| `IRONCREW_CONVERSATIONS_DEFAULT_LIMIT` | Default page size for `GET /flows/{flow}/conversations` (default: `20`) |
| `IRONCREW_CONVERSATIONS_MAX_LIMIT` | Hard cap on the `limit` query parameter for `GET /flows/{flow}/conversations` (default: `100`) |
| `IRONCREW_CONVERSATION_MAX_HISTORY` | Default retained non-system messages for `crew:conversation({})` (default: `50`, hard ceiling: `4096`). Explicit zero/unbounded histories are rejected |
| `IRONCREW_DIALOG_MAX_HISTORY` | Default retained dialog turns (default: `100`, hard ceiling: `4095`; one of 4096 provider-history slots is reserved for the starter). Explicit zero is rejected |
| `IRONCREW_DIALOG_MAX_TURNS` | Maximum accepted turns for one dialog (default: `1000`, hard ceiling: `10000`) |
| `IRONCREW_DIALOG_MAX_PARTICIPANTS` | Maximum accepted dialog participants (default: `16`, hard ceiling: `64`) |
| `IRONCREW_MAX_FLOW_DEPTH` | Max recursive nesting for `run_flow()` / `crew:subworkflow()` (default: `5`). Exceeding it fails with a validation error |
| `IRONCREW_API_CONVERSATION_MAX_HISTORY` | Server-side ceiling applied to an HTTP conversation's requested `max_history` (default: `50`, hard ceiling: `1000`) |
| `IRONCREW_API_MAX_TAGS` | Maximum tags accepted on one HTTP flow run (default: `32`; hard ceiling: `256`) |
| `IRONCREW_API_MAX_TAG_BYTES` | Maximum UTF-8 bytes in one non-empty, trimmed HTTP run tag (default: `256`; hard ceiling: `4096`) |
| `IRONCREW_API_MAX_TAGS_BYTES` | Maximum aggregate bytes across one HTTP run's tags (default: `4096`; hard ceiling: `65536`). Duplicate and control-character tags are rejected |
| `IRONCREW_API_MESSAGE_MAX_BYTES` | Maximum UTF-8 bytes in one HTTP conversation message (default: `262144`, hard ceiling: `4194304`) |
| `IRONCREW_API_MAX_IMAGES_PER_MESSAGE` | Maximum image locators on one HTTP message (default: `4`, hard ceiling: `32`) |
| `IRONCREW_API_MAX_IMAGES_PER_CONVERSATION` | Maximum cumulative image count retained by one live conversation (default: `16`, hard ceiling: `256`) |
| `IRONCREW_API_MAX_IMAGE_BYTES_PER_MESSAGE` | Maximum decoded image bytes accepted on one HTTP message (default: `20971520`, hard ceiling: `104857600`) |
| `IRONCREW_API_MAX_IMAGE_BYTES_PER_CONVERSATION` | Maximum cumulative decoded image bytes retained by one conversation (default: `33554432`, hard ceiling: `536870912`) |
| `IRONCREW_API_MAX_IMAGE_LOCATOR_BYTES` | Maximum bytes in one path, URL, or data-URL image locator (default: `2048`, hard ceiling: `16384`) |

**API Server:**

| Variable          | Description |
|-------------------|-------------|
| `IRONCREW_API_TOKEN` | Bearer token for REST API auth. Public binds require it unless the explicit unsafe override below is enabled. Tokens must be 32–4096 visible ASCII bytes without spaces. Health routes remain public |
| `IRONCREW_API_PRINCIPAL` | Stable audit/quota principal label for `IRONCREW_API_TOKEN` (default: `default`; 1–128 restricted ASCII bytes). Authenticated requests overwrite caller-supplied `X-Audit-Actor` with this trusted label |
| `IRONCREW_API_TOKENS` | Optional JSON object mapping up to 256 named principals to bearer-token strings. The legacy token counts toward the same 256-token process limit; duplicate tokens or principal names fail startup |
| `IRONCREW_ALLOW_UNAUTHENTICATED` | Explicit `true`/`1` override permitting a public bind without either API token source; unsafe for production and logged as a warning |
| `IRONCREW_ADMISSION_WORK_RATE_PER_MINUTE` | Per-principal process-local refill rate for run, conversation-start, and conversation-message mutations (default: `60`; range: 1–60000) |
| `IRONCREW_ADMISSION_WORK_BURST` | Per-principal work-mutation burst capacity (default: `10`; range: 1–10000) |
| `IRONCREW_ADMISSION_CONTROL_RATE_PER_MINUTE` | Independent per-principal refill rate for abort, answer, and delete control mutations (default: `120`; range: 1–60000) |
| `IRONCREW_ADMISSION_CONTROL_BURST` | Per-principal control-mutation burst capacity (default: `20`; range: 1–10000) |
| `IRONCREW_ADMISSION_OBSERVATION_RATE_PER_MINUTE` | Independent per-principal process-local refill rate for `GET /flows/{flow}/questions/{run_id}` polling (default: `600`; range: 1–60000). Does not govern internal durable-SSE database polling. |
| `IRONCREW_ADMISSION_OBSERVATION_BURST` | Per-principal question-poll burst capacity (default: `20`; range: 1–1000) |
| `IRONCREW_HOST` | Server bind host used when `--host` is absent. If neither is set, `PORT` implies `0.0.0.0`; otherwise the default is `127.0.0.1` |
| `IRONCREW_PORT` | Server bind port used when `--port` is absent. Takes precedence over platform `PORT` |
| `PORT` | Platform-provided server port fallback (including Railway). Causes the default host to become `0.0.0.0` |
| `IRONCREW_CORS_ORIGINS` | Comma-separated allowed origins (e.g., `https://app.example.com,https://admin.example.com`). Set to `*` for permissive. Absent = deny all |
| `IRONCREW_MAX_BODY_SIZE` | Max request body size in bytes (default: `10485760` = 10 MiB; range: 1–67108864) |
| `IRONCREW_MAX_CONVERSATION_TURN_SECS` | Whole conversation-turn deadline, including provider and tool rounds (default: `300`; hard ceiling: `3600`) |
| `IRONCREW_MAX_RUN_LIFETIME` | Max run duration in seconds for API mode (default: `1800` = 30 min; hard ceiling: `86400`) |
| `IRONCREW_REQUIRE_IDEMPOTENCY_KEY` | Require exactly one valid `Idempotency-Key` on HTTP runs and JSON/SQLite conversation messages (default: `false`; recommended: `true` in production). PostgreSQL conversation messages require the header regardless because it is their shared turn fence |
| `IRONCREW_IDEMPOTENCY_TTL_SECONDS` | Completed/indeterminate ledger retention (default: `86400`; range: 60–2592000; must be at least `IRONCREW_MAX_RUN_LIFETIME + 3600`) |
| `IRONCREW_IDEMPOTENCY_MAX_RECORDS` | Maximum in-flight plus retained terminal request records (default: `10000`; hard ceiling: `100000`) |
| `IRONCREW_IDEMPOTENCY_MAX_RECORDS_PER_PRINCIPAL` | Maximum in-flight plus retained terminal records charged to one authenticated principal (default: global record cap; cannot exceed it) |
| `IRONCREW_IDEMPOTENCY_MAX_IN_FLIGHT_PER_PRINCIPAL` | Maximum concurrent claimed/in-progress mutations charged to one principal (default: `min(global record cap, 64)`; cannot exceed the principal record cap) |
| `IRONCREW_IDEMPOTENCY_PRUNE_BATCH` | Maximum expired terminal records removed in one bounded pass (default: `1000`; hard ceiling: `10000`) |
| `IRONCREW_IDEMPOTENCY_MAX_RESPONSE_BYTES` | Maximum compact JSON response retained per key (default: `8388608` = 8 MiB; hard ceiling: 64 MiB) |
| `IRONCREW_IDEMPOTENCY_MAX_TOTAL_RESPONSE_BYTES` | Aggregate retained response-body budget (default: `268435456` = 256 MiB; hard ceiling: 8 GiB); excess responses become non-replayable tombstones |
| `IRONCREW_IDEMPOTENCY_MAX_TOTAL_RESPONSE_BYTES_PER_PRINCIPAL` | Aggregate retained response-body budget charged to one principal (default: global response-byte cap; cannot exceed it); excess responses become non-replayable tombstones |
| `IRONCREW_MAX_SSE_CONNECTIONS` | Process-wide per-replica cap on live run and conversation SSE connections (default: `16`; hard ceiling: `1024`) |
| `IRONCREW_READINESS_CACHE_MS` | Storage-aware readiness result cache interval (default: `1000`; hard ceiling: `10000`). Overlapping uncached probes wait up to one second for the shared check, then fail closed. |
| `IRONCREW_RUN_SSE_RETENTION_SECS` | Time a completed run's event bus remains available for a late subscriber (default: `5`; hard ceiling: `300`) |
| `IRONCREW_SSE_OUTPUT_MAX_CHARS` | Truncate task output in process-local JSON/SQLite SSE responses to N chars (disabled by default). PostgreSQL journal payloads are bounded at emission by `IRONCREW_EVENT_MAX_BYTES`; sanitize sensitive flow output before emitting it. |
| `IRONCREW_SHUTDOWN_ROUTING_GRACE_SECS` | Routing deadline measured from SIGTERM/Ctrl+C (default: `5`; range: 0–300 seconds). Fencing consumes part of it and any remainder is spent in `draining`; fence failures retry with bounded store attempts and exponential backoff from 100 ms capped at 5 seconds beyond the deadline, blocking `stopping`. A successful `SIGUSR1` remains draining until a later termination signal. |
| `IRONCREW_SHUTDOWN_TIMEOUT_SECS` | Hard teardown deadline in seconds started when the lifecycle enters `stopping`. If graceful teardown has not completed by then, the process exits anyway (default: `10`; range: 1–300) |
| `IRONCREW_SHUTDOWN_DRAIN_MS` | Post-serve drain window in milliseconds for background tasks spawned from `Drop` paths (notably reaping stdio MCP children) (default: `1000`; range: 0–30000) |

Admission buckets are intentionally process-local. With multiple HTTP
replicas, multiply process-local capacity and the per-pod RAM allocation by
replica count; shared PostgreSQL idempotency and global journal budgets do not
multiply. PostgreSQL's keyed cancellation/HITL coordination does not make
process admission global. Put any required cluster-wide request or provider
budget in a trusted shared gateway with bounded queues and idempotency-key
preservation. Rate-limit breaches return `429` with numeric
`Retry-After` and `Cache-Control: no-store`; the independent control bucket
keeps abort/answer/delete operations available when work admission is busy,
and the observation bucket prevents aggressive question-list polling from
consuming either mutation bucket. Durable SSE's internal PostgreSQL page reads
are instead bounded by the SSE connection, page, poll, read-timeout, and pool
settings.
Use the explicit formulas and caveats in [HTTP Scaling](http-scaling.md#multi-replica-capacity-arithmetic)
before changing replica count.
`GET /metrics` is protected by the same API authentication and exposes only
fixed, low-cardinality labels—never principal names, bearer tokens, audit
actors, flow/task/tool names, URLs, errors, prompts, provider output, or
idempotency keys. In addition to build, admission, resource, and durable-ledger
utilization, it exports process-local execution counters and duration
histograms for runs, tasks, tools, and provider requests; token, SSE, lease-loss,
reconciliation, terminal-persistence, and instrumented store-failure counters.
The exact new series and closed labels are listed in
[REST API: GET /metrics](rest-api.md#get-metrics). Two unlabeled instantaneous
gauges surface the readiness-critical storage signals already maintained by
each process:

- `ironcrew_store_maintenance_healthy` is `1` when the latest completed
  heartbeat-plus-reconciliation maintenance cycle succeeded and `0` after a
  failed startup/cycle until a complete cycle recovers.
- `ironcrew_process_terminal_persistence_degraded_finalizers` is the current
  number of run or conversation finalizers retrying durable persistence. It is
  not a cumulative failure counter and returns to `0` as those finalizers
  recover or are fenced.
- `ironcrew_process_lifecycle_state{state="..."}` is a one-hot gauge over the
  four fixed lifecycle states.
- `ironcrew_process_lifecycle_rejections_total{class="work|control"}` counts
  direct protected mutations rejected after this process withdrew.

Unlabeled resource gauges also expose process-only Linux `VmRSS`/`VmHWM` with
an availability flag, this process's SQLx PostgreSQL pool, logical provider
futures (including pacing), registered EventBus replay retention/capacities,
and active/limit SSE permits. They are fixed-cardinality and per scrape target;
they are not cgroup/OOM, child-process, PostgreSQL-server, provider-side, or
cluster-global measurements. See
[Cloud Deployment: Metrics](cloud-deployment.md#metrics) for the exact series
and aggregation rules.

These process series reset with the process. In a multi-replica deployment,
scrape and alert on every pod. Sum counter rates across unique targets and
aggregate histogram buckets by `le` plus their fixed dimensions before calling
`histogram_quantile`; do not sum the maintenance-health boolean, duplicate the
shared durable-ledger snapshot, or assume a load-balanced scrape sampled every
replica. Provider token counters advance only when reported by a successful
provider response and are not billing data.

**Security:**

| Variable          | Description |
|-------------------|-------------|
| `IRONCREW_ALLOW_PRIVATE_IPS` | Set to `1` or `true` to allow protected HTTP clients to reach private/internal addresses. Unset keeps DNS resolution, actual connections, and redirect targets restricted to public addresses |
| `IRONCREW_ENV_ALLOWLIST` | Comma-separated exact env var names Lua `env()` may read. Fail-closed: every name not listed returns `nil`. |
| `IRONCREW_TRUST_PROXY` | Set to `1` to honor `X-Forwarded-For` for source-IP capture in audit events (only enable when running behind a trusted reverse proxy) |
| `IRONCREW_AUDIT_DEFAULT_LIMIT` | Default page size on `GET /audit` (default `50`) |
| `IRONCREW_AUDIT_MAX_LIMIT` | Hard cap on `GET /audit?limit=` (default `500`) |

**Tool Resource Budgets:**

| Variable          | Description |
|-------------------|-------------|
| `IRONCREW_HTTP_MAX_REQUEST_HEADER_BYTES` | Aggregate explicit and generated request-header budget for `http_request` (default: `65536` = 64 KiB; hard ceiling: `1048576` = 1 MiB). A request accepts at most 128 headers |
| `IRONCREW_HTTP_MAX_REQUEST_BODY_BYTES` | Request-body cap for `http_request` (default: `8388608` = 8 MiB; hard ceiling: `67108864` = 64 MiB) |
| `IRONCREW_HTTP_MAX_RESPONSE_BYTES` | Primary response-body cap shared by `http_request` and Lua `http.*` (default: `8388608` = 8 MiB). `IRONCREW_MAX_RESPONSE_SIZE` is consulted only as a deprecated fallback when this variable is absent |
| `IRONCREW_HTTP_MAX_HEADER_BYTES` | Aggregate response-header cap for protected HTTP tools (default: `65536` = 64 KiB) |
| `IRONCREW_HTTP_MAX_JSON_BYTES` | Largest HTTP body auto-parsed into an additional JSON tree (default: `2097152` = 2 MiB) |
| `IRONCREW_HTTP_MAX_OUTPUT_BYTES` | Maximum serialized `http_request` result after JSON escaping/formatting (default: `16777216` = 16 MiB) |
| `IRONCREW_WEB_SCRAPE_MAX_BYTES` | Max HTML body size for the `web_scrape` tool, in bytes. Streamed and capped before DOM parse. Default: `2097152` (2 MiB) |
| `IRONCREW_MAX_IMAGE_BYTES` | Per-image cap for local and remote image inputs (default: `20971520` = 20 MiB) |
| `IRONCREW_PROVIDER_MAX_RESPONSE_BYTES` | Maximum non-streaming provider response body (default: `16777216` = 16 MiB) |
| `IRONCREW_PROVIDER_MAX_ERROR_BYTES` | Maximum provider/remote-image error body retained (default: `262144` = 256 KiB) |
| `IRONCREW_PROVIDER_MAX_STREAM_BYTES` | Maximum raw provider SSE bytes consumed for one streaming response (default: `33554432` = 32 MiB) |
| `IRONCREW_PROVIDER_MAX_OUTPUT_BYTES` | Maximum accumulated provider text/reasoning output (default: `16777216` = 16 MiB) |
| `IRONCREW_CHAT_HISTORY_MAX_BYTES` | Maximum estimated in-memory bytes retained in one provider chat history (default: `33554432` = 32 MiB; hard ceiling: `268435456`) |
| `IRONCREW_MAX_REASONING_BYTES` | Maximum reasoning/thinking text retained across one provider tool loop (default: `1048576` = 1 MiB; hard ceiling: `16777216`) |
| `IRONCREW_FILE_READ_MAX_BYTES` | Max bytes read by `file_read` or per file in `file_read_glob` (default: `10485760` = 10 MiB; hard ceiling: `268435456` = 256 MiB) |
| `IRONCREW_FILE_WRITE_MAX_BYTES` | Max bytes written by one `file_write` call (default: `10485760` = 10 MiB; hard ceiling: `268435456` = 256 MiB) |
| `IRONCREW_FILE_WRITE_ROOT` | Capability root for `file_write` and custom-tool `fs.write`. Public binds require an explicit absolute path disjoint from the flow source tree |
| `IRONCREW_GLOB_MAX_FILES` | Max number of matching files considered by `file_read_glob` (default: `500`; hard ceiling: `10000`; zero/invalid values use the default) |
| `IRONCREW_GLOB_MAX_BYTES` | Max aggregate file-content bytes returned by `file_read_glob` (default: `52428800` = 50 MiB; hard ceiling: `268435456` = 256 MiB; zero/invalid values use the default) |
| `IRONCREW_GLOB_MAX_ENTRIES` | Maximum regular filesystem entries scanned before glob matching (default: `10000`; hard ceiling: `100000`) |
| `IRONCREW_GLOB_MAX_OUTPUT_BYTES` | Maximum final serialized `file_read_glob` JSON output, including escaping and metadata (default: `67108864` = 64 MiB; hard ceiling: `268435456` = 256 MiB) |
| `IRONCREW_SHELL_TIMEOUT_SECS` | Default shell command deadline (default: `60`; range: 1–3600). A call-level `timeout_secs` can override it within the same range |
| `IRONCREW_SHELL_MAX_OUTPUT_BYTES` | Max bytes captured per stream (stdout and stderr independently) by the `shell` tool (default: `1048576`; range: 1–16777216) |
| `IRONCREW_JSON_SCHEMA_MAX_BYTES` | Maximum serialized JSON Schema accepted by `validate_schema` (default: `262144`; range: 1024–4194304). External `$ref` retrieval is disabled; local `#` fragments remain supported |
| `IRONCREW_ASK_HUMAN_TIMEOUT` | Default question timeout when a flow omits `timeout_s` (default: `600`) |
| `IRONCREW_ASK_HUMAN_MAX_TIMEOUT` | Maximum accepted per-question timeout (default: `3600`; hard ceiling: `86400` seconds) |
| `IRONCREW_ASK_HUMAN_MAX_PENDING` | Maximum simultaneously pending questions per run (default: `16`; hard ceiling: `256`) |
| `IRONCREW_ASK_HUMAN_MAX_PENDING_BYTES` | Maximum aggregate serialized metadata retained for pending questions in one run (default: `1048576` = 1 MiB; hard ceiling: `16777216` = 16 MiB). PostgreSQL permits the same ciphertext budget plus 28 AEAD-overhead bytes per allowed row. |
| `IRONCREW_ASK_HUMAN_MAX_PROMPT_BYTES` | Maximum question prompt bytes (default: `65536`; hard ceiling: `1048576`) |
| `IRONCREW_ASK_HUMAN_MAX_CHOICES` | Maximum choices on one question (default: `100`; hard ceiling: `1000`) |
| `IRONCREW_ASK_HUMAN_MAX_CHOICES_BYTES` | Maximum aggregate bytes across choices (default: `65536`; hard ceiling: `1048576`) |
| `IRONCREW_ASK_HUMAN_MAX_ANSWER_BYTES` | Maximum serialized answer bytes (default: `65536`; hard ceiling: `1048576`) |
| `IRONCREW_HITL_ENCRYPTION_KEYS` | Secret JSON object mapping at most 8 key ids to canonical base64 encodings of 32-byte keys (maximum JSON size: 16384 bytes). Together with an active key id, enables encrypted PostgreSQL cross-replica questions and answers for idempotency-keyed HTTP runs. It is loaded once at process startup. During rotation, every process must have the expanded old+new set before active ids differ; remove old only after all old-active writers exit and both mailbox fingerprint columns have zero old references. Startup fails if retained ciphertext needs an absent key. |
| `IRONCREW_HITL_ACTIVE_KEY_ID` | Key id from `IRONCREW_HITL_ENCRYPTION_KEYS` used for newly registered question ciphertext. Answers inherit their authenticated question's key. Both variables must be set together; malformed or partial configuration fails PostgreSQL store startup. |
| `IRONCREW_HITL_POLL_INTERVAL_MS` | Owner mailbox polling interval per pending durable question (default: `500`; effective range: 50–5000 ms). Lower values reduce answer latency but multiply PostgreSQL reads across pending questions, runs, and replicas. |
| `IRONCREW_HITL_READ_TIMEOUT_MS` | Deadline for one owner-side durable-answer PostgreSQL read (default: `2000`; effective range: 100–30000 ms). Timeouts are retried after the poll interval until the question deadline. |
| `IRONCREW_HITL_PG_MAX_CONCURRENT_READS` | Per-process cap shared by concurrent PostgreSQL question-list decryption and answer-side question authentication (default: `8`; range: 1–64). Exhaustion fails closed instead of queuing unbounded ciphertext materialization. |

The HTTP, web-scrape, image, and provider-body/output byte settings use a
shared process hard cap of `268435456` (256 MiB); zero, invalid, or excessive
values fall back to their documented safe defaults. Other rows use the
individual ranges stated in their descriptions.

**MCP (Model Context Protocol):**

| Variable          | Description |
|-------------------|-------------|
| `IRONCREW_MCP_ALLOWED_COMMANDS` | Comma-separated exact command strings allowed for stdio MCP servers (e.g. `"uvx,npx"`). Basenames do not authorize lookalike paths. Unset retains the development allow-all default; present-but-empty fails closed |
| `IRONCREW_MCP_ALLOWED_HTTP_HOSTS` | Comma-separated exact HTTP MCP hostnames, or `__disabled__`. Public binds require this policy because rmcp transport frames are materialized before IronCrew's post-decode caps; enable only operator-trusted hosts |
| `IRONCREW_MCP_ALLOW_LOCALHOST` | Narrow opt-in for literal localhost/loopback HTTP MCP URLs. Default: blocked. The broader `IRONCREW_ALLOW_PRIVATE_IPS` override supersedes this policy and should not be used merely for a local sidecar |
| `IRONCREW_MCP_HANDSHAKE_TIMEOUT_SECS` | MCP connection deadline (default: `10`; range: 1–3600 seconds) |
| `IRONCREW_MCP_LIST_TIMEOUT_SECS` | Tool-discovery deadline (default: `10`; range: 1–3600 seconds) |
| `IRONCREW_MCP_CALL_TIMEOUT_SECS` | Tool-call deadline (default: `60`; range: 1–3600 seconds) |
| `IRONCREW_MCP_SHUTDOWN_TIMEOUT_SECS` | Graceful MCP shutdown deadline (default: `5`; range: 1–3600 seconds) |
| `IRONCREW_MCP_MAX_TOOLS` | Maximum tools advertised by one server (default: `128`; hard ceiling: `4096`) |
| `IRONCREW_MCP_MAX_LIST_PAGES` | Maximum discovery pages (default: `32`; hard ceiling: `256`) |
| `IRONCREW_MCP_MAX_TOOL_DEFINITION_BYTES` | Maximum serialized discovered tool definition (default: `131072`; hard ceiling: `1048576`) |
| `IRONCREW_MCP_MAX_TOOL_DESCRIPTION_BYTES` | Maximum description bytes accepted by the bridge (default: `16384`; hard ceiling: `1048576`) |
| `IRONCREW_MCP_MAX_TOOL_SCHEMA_BYTES` | Maximum input-schema bytes accepted by the bridge (default: `65536`; hard ceiling: `1048576`) |
| `IRONCREW_MCP_TOOL_ARGUMENT_MAX_BYTES` | Maximum serialized arguments for one MCP call (default: `262144`; hard ceiling: `4194304`) |
| `IRONCREW_MCP_MAX_CONTENT_ITEMS` | Maximum content blocks in one MCP result (default: `256`; hard ceiling: `4096`) |
| `IRONCREW_MCP_TOOL_RESULT_MAX_BYTES` | Maximum flattened text bytes returned per MCP tool call (default: `262144`; hard ceiling: `16777216`). Oversized text is UTF-8-safely truncated |

**Storage:**

| Variable          | Description |
|-------------------|-------------|
| `IRONCREW_STORE`    | Storage backend: exactly `json`, `sqlite`, or `postgres` (`json` only when absent). Public server binds require this to be explicit; unknown values fail startup |
| `IRONCREW_STORE_PATH` | Path for SQLite database file (default: `<flow>/.ironcrew/ironcrew.db`) |
| `DATABASE_URL` | PostgreSQL 15+ connection string (required when `IRONCREW_STORE=postgres`) |
| `IRONCREW_PG_TABLE_PREFIX` | Table prefix for shared PostgreSQL databases (e.g., `myapp_` → `myapp_runs`), at most 37 lowercase ASCII alphanumeric/underscore bytes |
| `IRONCREW_DB_POOL_SIZE` | PostgreSQL connection pool size (default: `10`; range: 1–128) |
| `IRONCREW_DB_CONNECT_RETRIES` | PostgreSQL connection retries after the initial attempt (default: `10`; range: 0–100) |
| `IRONCREW_DB_CONNECT_BACKOFF_MS` | Base delay for exponential PostgreSQL connection-retry backoff, in milliseconds (default: `1000`; range: 1–30000) |
| `IRONCREW_DB_CONNECT_TIMEOUT_SECS` | PostgreSQL connect/acquire timeout (default: `30`; range: 1–120 seconds) |
| `IRONCREW_INSTANCE_ID` | Optional unique 1–255 byte printable ASCII process/pod identity stored as the owner of live runs. Generated once per process when unset |
| `IRONCREW_RUN_LEASE_TTL_SECONDS` | Seconds before an unrefreshed run lease is eligible for abandoned-run reconciliation; also the mandatory grace before explicit recovery of an indeterminate conversation turn (default: `60`, production range: 6–86400). Owner heartbeats and replica maintenance run every TTL/3 |
| `IRONCREW_JSON_STORE_RECORD_MAX_BYTES` | Maximum bytes read/written for one JSON run record (default: `67108864` = 64 MiB; hard ceiling: `134217728`) |
| `IRONCREW_JSON_STORE_MAX_SCAN_ENTRIES` | Maximum run files visited by one JSON-store list/count/clean scan (default: `10000`; hard ceiling: `100000`) |
| `IRONCREW_RUNS_DEFAULT_LIMIT` | Default page size for `GET /flows/{flow}/runs` when `limit` is not provided. Default: `20` |
| `IRONCREW_RUNS_MAX_LIMIT` | Hard cap on `limit` for `GET /flows/{flow}/runs`. A client asking for more is silently clamped. Default: `100` |

For PostgreSQL, each broad owner heartbeat and reconciliation operation has a
bounded transaction-local lock/statement timeout and a slightly larger outer
watchdog. The limits are derived from the lease cadence and cap at 4 seconds
per statement inside PostgreSQL and 5 seconds for the aggregate operation at
the default TTL. Reconciliation processes at most 64 runs per cycle and keeps
only those IDs in application memory. A dead owner is therefore nominally
detected within the TTL, one additional cadence, and one bounded
heartbeat-plus-reconciliation cycle when it fits in the next batch. Backlog or
repeated database failures can extend that window; this mechanism records
`abandoned` work and never resumes execution on another process. See
[Storage Backends](storage.md#run-ownership-and-terminal-writes) for the exact
formula and minimum-TTL example.

**Crew memory:**

| Variable | Description |
|---|---|
| `IRONCREW_MEMORY_MAX_KEY_BYTES` | Maximum memory-item key bytes (default: `1024`) |
| `IRONCREW_MEMORY_MAX_VALUE_BYTES` | Maximum serialized value bytes (default: `1048576` = 1 MiB) |
| `IRONCREW_MEMORY_MAX_TAGS` | Maximum tags on one item (default: `32`) |
| `IRONCREW_MEMORY_MAX_TAG_BYTES` | Maximum bytes in one tag (default: `256`) |
| `IRONCREW_MEMORY_PERSIST_MAX_BYTES` | Maximum persistent memory file/snapshot bytes (default: `16777216` = 16 MiB) |
| `IRONCREW_MEMORY_CONTEXT_MAX_BYTES` | Maximum aggregate memory context injected into a task prompt (default: `65536`) |
| `IRONCREW_MEMORY_QUERY_MAX_BYTES` | Maximum memory-query bytes (default: `16384`) |

### .env File Loading

1. The `.env` file in the **current working directory** is loaded first.
2. The `.env` file in the **project directory** is loaded second and overrides
   any conflicting values from step 1.

---

## Verbose Mode

Pass `-v` on any command to set the log level to `debug`, overriding
`IRONCREW_LOG`:

```
ironcrew run . -v
```
