# Best Practices

Guidelines for building reliable, cost-effective, and maintainable IronCrew flows.

## Project Structure

A well-organized flow directory makes agents, tools, and the entrypoint easy to find.

```
my-flow/
  agents/
    researcher.lua
    writer.lua
  tools/
    summarize.lua
  crew.lua            # entrypoint
  .env                # API keys (never commit this)
```

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

## Error Handling

**`on_error` routing.** Set `on_error` on a task to name another task that should
run when the primary task fails. This lets you build fallback chains.

**Retries.** Set `max_retries` and `retry_backoff_secs` on tasks that may fail
due to transient issues (rate limits, network errors). The engine emits
`task_retry` events so you can monitor retry behavior.

**Timeouts.** Set `timeout_secs` on long-running tasks to prevent them from
blocking the entire crew. The server also enforces a 30-minute maximum run
lifetime.

**Conditions.** Use `condition` to skip tasks based on previous results. A task
with a false condition emits a `task_skipped` event and does not count as failed.

## Performance

**Parallel execution.** Structure tasks so that independent work runs concurrently.
Tasks in the same dependency phase execute in parallel. Use `max_concurrent` on
the crew to limit parallelism if needed.

**Model routing.** Use cheap models (`gpt-4o-mini`, `gemini-2.5-flash`) for simple
tasks and reserve capable models for reasoning. Set `models` on the crew or
`model` on individual agents and tasks.

**Streaming.** Enable `stream = true` on the crew or individual tasks to get
LLM output as it arrives.

## Security

**Path validation.** The API server rejects flow identifiers with path traversal
components (`..`, absolute paths, multi-component paths). Only single directory
names within `--flows-dir` are accepted.

**Sandbox restrictions.** The Lua runtime operates in a sandboxed environment.
Custom tools define their own parameter schemas, and built-in tools like
`file_read` and `file_write` can be scoped to specific directories.

**API keys in environment variables.** Store API keys in `.env` files or
environment variables, never in Lua scripts or HTTP bodies. Use `env("KEY")`
in Lua. Add `.env` to `.gitignore` and `.dockerignore`.

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
  -v ./flows:/app/flows \
  ironcrew serve --host 0.0.0.0 --port 3000 --flows-dir /app/flows
```

Bind to `0.0.0.0` inside the container so the port mapping works.

## Cost Optimization

**Prompt caching.** Enable `prompt_cache_key` on crews with repetitive system
prompts. Cached tokens are tracked in run records.

**Token tracking.** Monitor `total_tokens` and `cached_tokens` in run records.
Use the SSE `task_completed` event for per-task breakdowns.

**Model routing.** Route cheap tasks to fast models and expensive tasks to
capable models. This is the highest-impact optimization for multi-task crews.

**Small prompts.** Keep system prompts and descriptions concise -- every token
in the system prompt is repeated on every LLM call for that agent.
