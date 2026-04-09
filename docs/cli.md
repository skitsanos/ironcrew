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
`tools/*.lua`, and `crew.lua`. Run history is saved to `.ironcrew/runs/`.
- In `--json` mode, Lua `print()` calls are suppressed and the full run record
(status, tasks, token usage) is written to stdout as JSON. Tracing logs go to
stderr, so piping works cleanly.

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

List all 9 built-in tools with their descriptions.

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
| `--host`       | `127.0.0.1`   | Bind address |
| `--port`       | `3000`        | Bind port |
| `--flows-dir`  | `.`           | Directory containing crew flow subdirectories |

**Endpoints:**

| Method | Path                             | Description |
|--------|----------------------------------|-------------|
| GET    | `/health`                        | Health check |
| POST   | `/flows/{flow}/run`              | Run a crew (async, returns run_id) |
| GET    | `/flows/{flow}/events/{run_id}`  | SSE event stream for a run |
| GET    | `/flows/{flow}/runs`             | List past runs for a flow |
| GET    | `/flows/{flow}/runs/{id}`        | Get run details |
| DELETE | `/flows/{flow}/runs/{id}`        | Delete a run record |
| GET    | `/flows/{flow}/validate`         | Validate a flow |
| GET    | `/flows/{flow}/agents`           | List agents in a flow |
| GET    | `/nodes`                         | List built-in tools |

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
| Tool summary | Lists custom tools alongside the 9 built-in tools |
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
| `-s, --status` | (all)   | Filter by status: `success`, `partial_failure`, `failed` |
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
| `ANTHROPIC_API_KEY` | Required for `provider = "anthropic"`; auto-resolved when `base_url` contains `anthropic.com` |
| `GEMINI_API_KEY`  | Auto-resolved when `base_url` contains `googleapis.com` or `gemini` |
| `GROQ_API_KEY`    | Auto-resolved when `base_url` contains `groq.com` |
| `MOONSHOT_API_KEY` | Auto-resolved when `base_url` contains `moonshot.ai` or `moonshot.cn` (Kimi K2.5) |
| `DEEPSEEK_API_KEY` | Auto-resolved when `base_url` contains `deepseek.com` |
| `XAI_API_KEY`     | Auto-resolved when `base_url` contains `x.ai` (Grok) |
| `OPENROUTER_API_KEY` | Auto-resolved when `base_url` contains `openrouter.ai` |
| `IRONCREW_LOG`    | Log level filter (e.g., `info`, `debug`, `trace`, `warn`, `error`) |
| `IRONCREW_ALLOW_SHELL` | Set to `1` or `true` to enable the shell tool (disabled by default) |
| `IRONCREW_RATE_LIMIT_MS` | Minimum milliseconds between LLM API calls (e.g., `200` for 5 req/sec) |
| `IRONCREW_TOOL_TIMEOUT` | Max seconds a single tool execution may run (default: `60`) |
| `IRONCREW_DEFAULT_MAX_CONCURRENT` | Default max parallel tasks per phase when not set in crew config (default: `10`) |
| `IRONCREW_MAX_PROMPT_CHARS` | Max user prompt size in characters (default: `102400` = 100KB). Truncates with warning |
| `IRONCREW_MAX_EVENTS` | Max events (count) in the EventBus replay buffer (default: `1000`) |
| `IRONCREW_EVENT_REPLAY_MAX_BYTES` | Max total bytes in the replay buffer (default: `4194304` = 4 MB). Live SSE broadcasts are always lossless — this only affects the catch-up replay for late subscribers. `0` disables the cap. |
| `IRONCREW_CONVERSATION_MAX_HISTORY` | Default `max_history` for `crew:conversation({})` when not set explicitly (default: `50`). `0` means unbounded |
| `IRONCREW_DIALOG_MAX_HISTORY` | Default `max_history` for `crew:dialog({})` when not set explicitly (default: `100`). Applies to both the prompt window and the stored transcript. `0` means unbounded |
| `IRONCREW_MESSAGEBUS_QUEUE_DEPTH` | Max messages per agent queue in the MessageBus (default: `1000`). Oldest dropped on overflow with a warning log. `0` disables the cap |
| `IRONCREW_MESSAGEBUS_PENDING_CAP` | Max pending broadcasts (messages sent before any agent is registered) (default: `500`). `0` disables the cap |

**API Server:**

| Variable          | Description |
|-------------------|-------------|
| `IRONCREW_API_TOKEN` | Bearer token for REST API auth (disabled by default, `/health` always public) |
| `IRONCREW_CORS_ORIGINS` | Comma-separated allowed origins (e.g., `https://app.example.com,https://admin.example.com`). Set to `*` for permissive. Absent = deny all |
| `IRONCREW_MAX_BODY_SIZE` | Max request body size in bytes (default: `10485760` = 10MB) |
| `IRONCREW_MAX_RUN_LIFETIME` | Max run duration in seconds for API mode (default: `1800` = 30 min) |
| `IRONCREW_SSE_OUTPUT_MAX_CHARS` | Truncate task output in SSE events to N chars (disabled by default) |

**Security:**

| Variable          | Description |
|-------------------|-------------|
| `IRONCREW_ALLOW_PRIVATE_IPS` | Set to `1` to allow HTTP requests to private/loopback IPs (SSRF protection disabled) |
| `IRONCREW_ENV_BLOCKLIST` | Comma-separated additional env var names to block from Lua `env()` |

**Tool Resource Budgets:**

| Variable          | Description |
|-------------------|-------------|
| `IRONCREW_MAX_RESPONSE_SIZE` | Max HTTP response body size for the `http_request` tool, in bytes. Enforced both via `Content-Length` header and during streaming read. Default: `52428800` (50 MB) |
| `IRONCREW_WEB_SCRAPE_MAX_BYTES` | Max HTML body size for the `web_scrape` tool, in bytes. Streamed and capped before DOM parse. Default: `2097152` (2 MB) |
| `IRONCREW_FILE_READ_MAX_BYTES` | Max file size for the `file_read` tool, in bytes. Checked via metadata before reading. Default: `10485760` (10 MB) |
| `IRONCREW_GLOB_MAX_FILES` | Max number of files returned by `file_read_glob`. `0` disables the cap. Default: `500` |
| `IRONCREW_GLOB_MAX_BYTES` | Max total bytes aggregated by `file_read_glob` across all matched files. `0` disables the cap. Default: `52428800` (50 MB) |
| `IRONCREW_SHELL_MAX_OUTPUT_BYTES` | Max bytes captured per stream (stdout and stderr independently) by the `shell` tool. Overflow is discarded and a truncation marker is appended. Default: `1048576` (1 MB) |

**Storage:**

| Variable          | Description |
|-------------------|-------------|
| `IRONCREW_STORE`    | Storage backend: `json` (default), `sqlite`, or `postgres` |
| `IRONCREW_STORE_PATH` | Path for SQLite database file (default: `<flow>/.ironcrew/ironcrew.db`) |
| `DATABASE_URL` | PostgreSQL connection string (required when `IRONCREW_STORE=postgres`) |
| `IRONCREW_PG_TABLE_PREFIX` | Table prefix for shared PostgreSQL databases (e.g., `myapp_` → `myapp_runs`). Only alphanumeric and underscore allowed |
| `IRONCREW_DB_POOL_SIZE` | PostgreSQL connection pool size (default: `10`) |
| `IRONCREW_RUNS_DEFAULT_LIMIT` | Default page size for `GET /flows/{flow}/runs` when `limit` is not provided. Default: `20` |
| `IRONCREW_RUNS_MAX_LIMIT` | Hard cap on `limit` for `GET /flows/{flow}/runs`. A client asking for more is silently clamped. Default: `100` |

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
