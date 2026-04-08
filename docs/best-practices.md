# Best Practices

Patterns and tips for building crews that are reliable, fast, and cost-effective in production.

## Project Structure

A well-organized flow directory makes agents, tools, and the entrypoint easy to find.

```
my-flow/
  agents/
    researcher.lua
    writer.lua
  tools/
    summarize.lua
  config.lua          # project-wide defaults (optional)
  crew.lua            # entrypoint
  .env                # API keys (never commit this)
```

**Use `config.lua` for project defaults.** Put provider, model, concurrency,
and routing settings in a `config.lua` file at the project root so `crew.lua`
stays focused on workflow logic:

```lua
-- config.lua
return {
    provider = "anthropic",
    model = "claude-haiku-4-5-20251001",
    max_concurrent = 4,
    models = {
        task_execution = "claude-haiku-4-5-20251001",
        collaboration_synthesis = "claude-sonnet-4-5-20250929",
    },
}
```

Fields explicitly set in `Crew.new()` always win, so per-crew overrides still
work. This is the cleanest way to switch providers between dev and prod —
swap `config.lua` only.

For simple single-file flows, a standalone `crew.lua` works fine with inline
agent definitions. Use the directory structure when you have multiple agents,
custom tools, or want to share agent definitions across flows.

The `.ironcrew/` subdirectory is created automatically for run history and
persistent memory. Add it to `.gitignore`.

## Agent Design

**Focused roles.** Each agent should have a single, clear responsibility. An agent
named `researcher` with the goal "Find and summarize information from the web" is
better than a generic "do everything" agent.

**Clear goals.** Write goals as directives: "Analyze data and produce concise
summaries" rather than "This agent analyzes data."

**Specific capabilities.** List keywords in `capabilities` that match task
description vocabulary. The agent selector scores capability match (0.4),
tool match (0.3), and goal alignment (0.3).

**System prompts.** Use `system_prompt` for persistent instructions that apply to
every task the agent handles (output format, tone, constraints). Keep it concise --
long system prompts increase token costs on every request.

**Temperature.** Set `temperature` low (0.1-0.3) for deterministic extraction tasks
and higher (0.5-0.8) for creative generation. The default is provider-dependent.

## Task Design

**Small, focused tasks.** Break work into the smallest meaningful units. A task
that does one thing well is easier to debug, retry, and parallelize.

**Clear dependencies.** Use `depends_on` to define execution order. Tasks without
mutual dependencies run concurrently in the same phase (topologically sorted).

**Good descriptions.** The `description` is the primary LLM prompt. Be specific
about format: "in 2-3 sentences" or "as a JSON array."

**Expected output and context.** Set `expected_output` on agents for format hints.
Use `context` on tasks to inject additional information into the prompt.

## Stateful Conversations and Dialogs

Beyond single-shot tasks, IronCrew offers two stateful interaction primitives.
Choose the right shape for the work:

| Use case | Primitive | Why |
|----------|-----------|-----|
| Workflow with clear DAG | **Tasks** | Parallel execution, dependency phases, persisted to run records |
| Multi-turn chat with one agent | **`crew:conversation({})`** | Maintains message history, supports tool calls, captures reasoning |
| Two committed perspectives debating | **`crew:dialog({})`** | Perspective-flipped histories, each agent sees the other as "user" |
| Adversarial decision-making | **`crew:dialog()` + moderator** | Two debaters + a third agent that synthesizes structured output |

**The debate + moderator pattern** is the most productive use of dialogs:

1. Two agents with **opposing committed views** debate via `crew:dialog()`
2. Force each side to provide a **falsification criterion** per turn (use system prompts that mandate an `INVALIDATION:` line)
3. A third **moderator** agent reads the transcript via `crew:conversation()` and produces structured output
4. Use `response_format = "json_schema"` on the moderator so the synthesis is machine-readable

This pattern works for any binary decision under uncertainty: investment
analysis, code review (ship-it vs critic), architectural choices (microservices
vs monolith), hiring (hire vs pass), product decisions (build now vs wait).
See [`examples/stock-debate/`](../examples/stock-debate/) for a complete
implementation.

**Limitations** (current):
- Conversations and dialogs run intra-script — they do not persist across `crew:run()` calls
- Dialogs are two-agent only (multi-party round-robin is future work)

Both primitives emit dedicated SSE events (`conversation_*`, `dialog_*`)
through the EventBus, so REST API subscribers can stream conversation messages
and dialog turns in real time alongside task events.

## Error Handling

**`on_error` routing.** Set `on_error` on a task to name another task that should
run when the primary task fails. This lets you build fallback chains.

**Retries.** Set `max_retries` and `retry_backoff_secs` on tasks that may fail
due to transient issues (rate limits, network errors). The engine emits
`task_retry` events so you can monitor retry behavior.

**Timeouts.** Set `timeout_secs` on long-running tasks to prevent them from
blocking the entire crew. The server enforces a 30-minute maximum run lifetime
(`IRONCREW_MAX_RUN_LIFETIME`). The server handles `SIGTERM` and `Ctrl+C`
gracefully, allowing in-flight requests to complete before shutdown.

**Conditions.** Use `condition` to skip tasks based on previous results. A task
with a false condition emits a `task_skipped` event and does not count as failed.

## Performance

**Parallel execution.** Structure tasks so that independent work runs concurrently.
Tasks in the same dependency phase execute in parallel. A concurrency semaphore
always applies — crew `max_concurrent` > `IRONCREW_DEFAULT_MAX_CONCURRENT` env
var > default of 10. This prevents resource exhaustion in phases with many tasks.

**Model routing.** Use cheap models (`gpt-4.1-mini`, `gemini-2.5-flash`) for simple
tasks and reserve capable models for reasoning. Set `models` on the crew or
`model` on individual agents and tasks.

**Streaming.** Enable `stream = true` on the crew or individual tasks to get
LLM output as it arrives. When reasoning-capable providers are used
(Anthropic thinking, OpenAI Responses reasoning, DeepSeek/Kimi reasoning), the
reasoning deltas stream dim to stderr so you can watch the model's thought
process unfold.

**Reasoning/thinking.** For complex reasoning tasks, use
`provider = "anthropic"` with `thinking_budget` or
`provider = "openai-responses"` with `reasoning_effort = "medium"`. Both capture
the reasoning in the run record for later inspection. Reasoning tokens count as
output tokens and add cost, so reserve these for tasks that actually benefit
from deeper thinking.

**Server-side tools.** For research tasks, prefer built-in `web_search` via
Anthropic or OpenAI Responses over custom HTTP tools — no SSRF concerns, proper
citations, and one configuration flag instead of a whole tool implementation.

## Security

**CORS.** The API server denies cross-origin requests by default. Set
`IRONCREW_CORS_ORIGINS` to a comma-separated list of allowed origins, or `*`
for permissive access (development only). In production, always list specific
origins.

**SSRF protection.** The `http_request` tool and all Lua `http.*` globals block
requests to private/internal IP addresses (loopback, RFC1918, link-local, CGNAT)
by default. This prevents Lua scripts from probing internal networks. Override
with `IRONCREW_ALLOW_PRIVATE_IPS=1` if your agents legitimately need to reach
internal services.

**Environment variable security.** Lua `env()` blocks sensitive variables by
default: `DATABASE_URL`, `IRONCREW_API_TOKEN`, and any variable ending with
`_API_KEY`, `_SECRET`, `_TOKEN`, or `_PASSWORD`. Add custom names to
`IRONCREW_ENV_BLOCKLIST` (comma-separated). This prevents Lua scripts from
exfiltrating secrets into task output.

**Request/response size limits.** The server enforces a max request body size
(`IRONCREW_MAX_BODY_SIZE`, default 10MB). HTTP tools and Lua `http.*` enforce
a max response body size (`IRONCREW_MAX_RESPONSE_SIZE`, default 50MB). These
prevent memory exhaustion from oversized payloads.

**Prompt size limit.** User prompts (task description + context + dependency
results) are capped at `IRONCREW_MAX_PROMPT_CHARS` (default 100KB). Large
prompts are truncated with a warning to prevent OOM from large intermediate
outputs.

**Error sanitization.** API error responses do not expose filesystem paths or
internal server structure. Full details are logged server-side.

**Path validation.** The API server rejects flow identifiers with path traversal
components (`..`, absolute paths, multi-component paths). Only single directory
names within `--flows-dir` are accepted.

**Sandbox restrictions.** The Lua runtime operates in a sandboxed environment.
`io`, `debug`, `loadfile`, and `dofile` are removed. Custom tools define their
own parameter schemas, and built-in tools like `file_read` and `file_write` can
be scoped to specific directories.

**API keys in environment variables.** Store API keys in `.env` files or
environment variables, never in Lua scripts or HTTP bodies. Use `env("KEY")`
in Lua. Add `.env` to `.gitignore` and `.dockerignore`.

**Directory permissions.** The `.ironcrew/` directory is created with `0o700`
permissions on Unix, preventing other users from reading run history that may
contain sensitive task output.

## Storage Backends

**Default (JSON files).** By default, run records are stored as individual JSON
files under `<flow>/.ironcrew/runs/`. This requires no extra dependencies and
works well for development and moderate workloads.

**SQLite backend.** Set `IRONCREW_STORE=sqlite` to store run records in a SQLite
database instead. The database file defaults to `<flow>/.ironcrew/ironcrew.db`
but can be overridden with `IRONCREW_STORE_PATH`. SQLite is a good choice when
you have many runs and want faster queries or a single-file store.

**PostgreSQL backend.** Set `IRONCREW_STORE=postgres` with `DATABASE_URL` for
multi-instance cloud deployments. Pool size is configurable via
`IRONCREW_DB_POOL_SIZE` (default 10). Table prefix (`IRONCREW_PG_TABLE_PREFIX`)
allows sharing a database across projects — only alphanumeric and underscore are
allowed.

**Per-flow stores.** Each flow gets its own store instance based on its
`.ironcrew` directory. This keeps data isolated between flows regardless of the
backend.

**Switching backends.** Changing `IRONCREW_STORE` does not migrate existing data.
If you switch from `json` to `sqlite`, previously stored JSON runs remain in the
`runs/` directory but will not appear in queries against the SQLite store.

## Memory Management

**Default limits.** The memory store defaults to 500 items and 50,000 estimated
tokens. These limits prevent unbounded memory growth during long-running crews.

**Eviction.** When limits are exceeded, the store evicts items with the lowest
access count first, then by least recent update. Expired items (with TTL) are
removed before eviction runs.

**Persistent vs. ephemeral.** Use `memory = "persistent"` when you need memory
to survive across runs (stored in `.ironcrew/memory.json`). Use the default
`"ephemeral"` mode for throwaway scratch space.

**Custom limits.** Override defaults in the crew config:

```lua
local crew = Crew.new({
    goal = "My crew",
    memory = "persistent",
    max_memory_items = 1000,
    max_memory_tokens = 100000,
})
```

## Testing and Validation

**Validate command.** Use `ironcrew validate <flow>` or the
`GET /flows/{flow}/validate` endpoint to check syntax, agent definitions, and
tool references without running the flow.

**Verbose logging.** Set `IRONCREW_LOG=debug` for detailed output including full
LLM request/response bodies. Use `IRONCREW_LOG=info` for production.

**Incremental development.** Start with a single agent and one task. Verify it
works, then add agents and tasks incrementally. Check SSE events to understand
execution flow.

## Docker Deployment

**`.dockerignore`.** The project includes a `.dockerignore` that excludes `target/`,
`.git/`, `.env`, `docs/`, and other non-essential files from the build context.

**Multi-arch builds.** The Dockerfile uses `rust:latest` for building and
`debian:13-slim` for the runtime. Build for multiple architectures with:

```bash
docker buildx build --platform linux/amd64,linux/arm64 -t ironcrew .
```

**Environment file.** Pass API keys via `--env-file` rather than `-e` flags to
avoid leaking secrets in shell history:

```bash
docker run -p 3000:3000 \
  --env-file .env \
  -e IRONCREW_API_TOKEN=your-secret-token \
  -e IRONCREW_CORS_ORIGINS=https://app.example.com \
  -v ./flows:/app/flows \
  ironcrew serve --host 0.0.0.0 --port 3000 --flows-dir /app/flows
```

Bind to `0.0.0.0` inside the container so the port mapping works. Always set
`IRONCREW_API_TOKEN` and `IRONCREW_CORS_ORIGINS` in production deployments.

**Kubernetes.** The server handles `SIGTERM` gracefully, completing in-flight
requests before shutdown. Set `terminationGracePeriodSeconds` in your pod spec
to allow sufficient time for long-running crew executions to finish.

## Cost Optimization

**Prompt caching.** Enable `prompt_cache_key` on crews with repetitive system
prompts. Cached tokens are tracked in run records.

**Token tracking.** Monitor `total_tokens` and `cached_tokens` in run records.
Use the SSE `task_completed` event for per-task breakdowns.

**Model routing.** Route cheap tasks to fast models and expensive tasks to
capable models. This is the highest-impact optimization for multi-task crews.

**Small prompts.** Keep system prompts and descriptions concise -- every token
in the system prompt is repeated on every LLM call for that agent.
