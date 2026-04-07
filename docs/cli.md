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

List past run history for a project.

```
ironcrew runs -p .
ironcrew runs -p . --status success
```

| Flag           | Default | Description |
|----------------|---------|-------------|
| `-p, --project`| `.`     | Project path (locates `.ironcrew/runs/`) |
| `-s, --status` | (all)   | Filter by status: `success`, `partial_failure`, `failed` |

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

| Variable          | Description |
|-------------------|-------------|
| `OPENAI_API_KEY`  | Default API key for the OpenAI-compatible provider |
| `OPENAI_BASE_URL` | Default base URL (e.g., `https://api.openai.com/v1`) |
| `OPENAI_MODEL`    | Default model name (used in `.env` templates) |
| `GEMINI_API_KEY`  | Auto-resolved when `base_url` contains `googleapis.com` or `gemini` |
| `GROQ_API_KEY`    | Auto-resolved when `base_url` contains `groq.com` |
| `ANTHROPIC_API_KEY` | Auto-resolved when `base_url` contains `anthropic.com` |
| `IRONCREW_LOG`    | Log level filter (e.g., `info`, `debug`, `trace`, `warn`, `error`) |
| `IRONCREW_ALLOW_SHELL` | Set to `1` or `true` to enable the shell tool (disabled by default) |
| `IRONCREW_RATE_LIMIT_MS` | Minimum milliseconds between LLM API calls (e.g., `200` for 5 req/sec) |
| `IRONCREW_TOOL_TIMEOUT` | Max seconds a single tool execution may run (default: `60`) |
| `IRONCREW_MAX_RUN_LIFETIME` | Max run duration in seconds for API mode (default: `1800` = 30 min) |
| `IRONCREW_SSE_OUTPUT_MAX_CHARS` | Truncate task output in SSE events to N chars (disabled by default) |
| `IRONCREW_API_TOKEN` | Bearer token for REST API auth (disabled by default, `/health` always public) |
| `IRONCREW_STORE`    | Storage backend: `json` (default), `sqlite`, or `postgres` (feature-gated) |
| `IRONCREW_STORE_PATH` | Path for SQLite database file (default: `<flow>/.ironcrew/ironcrew.db`) |
| `DATABASE_URL` | PostgreSQL connection string (required when `IRONCREW_STORE=postgres`) |

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
