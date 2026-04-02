# IronCrew

A compiled Rust runtime for Lua-defined AI agent crews. Define agents, tasks, and orchestration logic in Lua, execute with a single native binary.

```lua
local crew = Crew.new({
    goal = "Research and summarize a topic",
    provider = "openai",
    model = env("OPENAI_MODEL") or "gpt-4o-mini",
})

crew:add_agent(Agent.new({
    name = "researcher",
    goal = "Find and analyze information",
    capabilities = {"research", "analysis"},
}))

crew:add_agent(Agent.new({
    name = "writer",
    goal = "Write clear summaries",
    capabilities = {"writing", "summarization"},
}))

crew:add_task({
    name = "research",
    description = "List 3 key benefits of Rust for systems programming",
})

crew:add_task({
    name = "summarize",
    description = "Summarize the research: ${results.research.output}",
    agent = "writer",
    depends_on = {"research"},
})

local results = crew:run()
```

## Features

- **Lua scripting** - Define agents, tasks, and tools in Lua. Rust handles the heavy lifting.
- **Parallel execution** - Independent tasks run concurrently within topological phases.
- **Provider-agnostic** - Works with OpenAI, Groq, Ollama, Azure, or any OpenAI-compatible API.
- **Structured output** - JSON Schema `response_format` forces LLMs to return valid structured data.
- **Built-in tools** - file_read, file_read_glob, file_write, web_scrape, shell, http_request, hash, template_render, validate_schema.
- **Custom Lua tools** - Define tools in Lua with access to `fs`, `env`, `regex`, `validate_json`.
- **Memory system** - Shared key-value store with TTL, relevance scoring, and persistent backend.
- **MessageBus** - Agent-to-agent communication with directed and broadcast messaging.
- **Collaborative tasks** - Multi-agent discussions with automatic synthesis.
- **Context interpolation** - `${results.task.output}` and `${env.VAR}` in task descriptions.
- **Retry + timeout** - Per-task retry with exponential backoff and configurable timeouts.
- **Conditional tasks** - Skip tasks based on Lua conditions evaluated against prior results.
- **Error recovery** - `on_error` routing to handler tasks with automatic recovery.
- **Subworkflows** - Compose crews from separate Lua files with input/output mapping.
- **Streaming** - Real-time LLM response output to stderr.
- **Model Router** - Route different task types to different models for cost optimization.
- **Token tracking** - Track prompt/completion/cached tokens per task and per run.
- **Prompt caching** - OpenAI prompt cache key and retention support for cost savings.
- **REST API** - Run crews via HTTP with run history and flow inspection.
- **SSE events** - Real-time Server-Sent Events stream for monitoring crew execution.
- **Run history** - Automatic persistence of run results with inspect/list/clean commands.
- **Schema validation** - Validate LLM JSON output against JSON Schema via tool or Lua global.
- **Foreach** - Iterate over lists and process each item as a task.
- **Single binary** - No runtime dependencies. Lua is vendored and compiled in.

## Quick Start

```bash
# Build from source
cargo build --release

# Initialize a new project
ironcrew init my-crew
cd my-crew

# Edit .env with your API key
echo "OPENAI_API_KEY=sk-..." > .env

# Run
ironcrew run .
```

## Project Structure

```
my-crew/
├── .env              # API keys (OPENAI_API_KEY, OPENAI_BASE_URL, etc.)
├── agents/
│   └── assistant.lua # Agent definitions (declarative)
├── tools/
│   └── custom.lua    # Custom tool definitions (optional)
└── crew.lua          # Entrypoint — orchestration logic
```

Agents and tools in their directories are auto-discovered. Everything can also be defined inline in `crew.lua`.

## Agent Definition

Inline in `crew.lua`:

```lua
crew:add_agent(Agent.new({
    name = "researcher",
    goal = "Find and analyze information on given topics",
    system_prompt = "You are a thorough researcher who cites sources.",
    capabilities = {"research", "analysis", "summarization"},
    tools = {"web_scrape", "file_write"},
    temperature = 0.3,
    model = "gpt-4o",
    max_tokens = 4000,
    response_format = {
        type = "json_schema",
        name = "research_result",
        schema = {
            type = "object",
            properties = {
                findings = { type = "array", items = { type = "string" } },
                sources = { type = "array", items = { type = "string" } },
            },
            required = {"findings", "sources"},
            additionalProperties = false,
        },
    },
}))
```

Or as a declarative file in `agents/researcher.lua` (auto-discovered):

```lua
return {
    name = "researcher",
    goal = "Find and analyze information on given topics",
    capabilities = {"research", "analysis"},
    temperature = 0.3,
}
```

## Task Options

```lua
crew:add_task({
    name = "research",
    description = "Research the topic: ${env.TOPIC}",
    agent = "researcher",              -- explicit assignment (or auto-selected)
    model = "gpt-4o",                  -- per-task model override
    expected_output = "A summary",     -- hint for the LLM
    context = "Additional context",    -- injected into prompt
    depends_on = {"setup"},            -- task dependencies
    max_retries = 3,                   -- retry on failure
    retry_backoff_secs = 1.0,          -- exponential backoff base
    timeout_secs = 120,                -- per-task timeout
    condition = "results.setup.success", -- skip if condition is false
    on_error = "fallback_task",        -- route to handler on failure
    stream = true,                     -- stream LLM output in real-time
})
```

## Crew Configuration

```lua
local crew = Crew.new({
    goal = "Your crew's objective",
    provider = "openai",
    model = "gpt-4o-mini",
    base_url = env("OPENAI_BASE_URL"),     -- for Groq, Ollama, etc.
    api_key = env("GROQ_API_KEY"),         -- per-crew API key override
    stream = true,                          -- stream all tasks
    max_concurrent = 4,                     -- limit parallel tasks
    memory = "persistent",                  -- "ephemeral" (default) or "persistent"
    max_memory_items = 100,                 -- eviction limit
    max_memory_tokens = 10000,              -- token-based eviction
    prompt_cache_key = "my-project-v1",     -- OpenAI prompt cache routing key
    prompt_cache_retention = "24h",         -- "in_memory" (default) or "24h"
    models = {                              -- Model Router: route by purpose
        task_execution = "gpt-4o-mini",
        collaboration = "gpt-4o",
        collaboration_synthesis = "gpt-4o",
    },
})
```

## Parallel Execution

Tasks with no dependencies run concurrently:

```lua
-- These three run in parallel (Phase 0)
crew:add_task({ name = "a", description = "..." })
crew:add_task({ name = "b", description = "..." })
crew:add_task({ name = "c", description = "..." })

-- This runs after all three complete (Phase 1)
crew:add_task({
    name = "combine",
    description = "Combine: ${results.a.output}, ${results.b.output}, ${results.c.output}",
    depends_on = {"a", "b", "c"},
})
```

## Collaborative Tasks

Multiple agents discuss a topic and synthesize a response:

```lua
crew:add_collaborative_task({
    name = "debate",
    description = "Should we adopt AI agents for code generation?",
    agents = {"optimist", "critic", "pragmatist"},
    max_turns = 2,
    depends_on = {"research"},
})
```

## Memory

Shared key-value store accessible to all tasks:

```lua
-- Store values
crew:memory_set("project", "IronCrew")
crew:memory_set_ex("temp_data", value, { tags = {"cache"}, ttl_ms = 60000 })

-- Read values
local project = crew:memory_get("project")

-- Task results are auto-stored as "task:name"
-- Memory context is auto-injected into agent prompts based on relevance
```

## MessageBus

Agent-to-agent communication:

```lua
-- Send directed message
crew:message_send("coordinator", "researcher", "Focus on Rust ecosystem", "notification")

-- Broadcast to all agents
crew:message_send("system", "*", "Keep responses concise", "broadcast")

-- Messages are auto-injected into agent prompts at task start
```

## Subworkflows

Compose crews from separate files:

```lua
local result = crew:subworkflow("sub/analysis.lua", {
    input = { topic = "Rust programming" },
    output_key = "analysis",
})
```

## Foreach

Iterate over a list and process each item:

```lua
crew:memory_set("topics", json_stringify({"Rust", "Python", "Go"}))

crew:add_foreach_task({
    name = "analyze_each",
    description = "Describe the main strength of ${item}",
    foreach = "topics",
    foreach_as = "item",
})
```

## Schema Validation

Validate LLM JSON output against a schema:

```lua
local results = crew:run()

for _, result in ipairs(results) do
    if result.task == "extract" and result.success then
        local check = validate_json(result.output, {
            type = "object",
            required = {"name", "findings"},
            properties = {
                name = { type = "string" },
                findings = { type = "array" },
            },
        })
        if not check.valid then
            log("warn", "Validation failed: " .. #check.errors .. " errors")
        end
    end
end
```

## Model Router

Route different task types to different models for cost optimization:

```lua
local crew = Crew.new({
    model = "gpt-4o-mini",           -- default for most tasks
    models = {
        task_execution = "gpt-4o-mini",
        collaboration = "gpt-4o",     -- use capable model for debates
        collaboration_synthesis = "gpt-4o",
    },
})

-- Per-task model override
crew:add_task({
    name = "complex_analysis",
    description = "...",
    model = "gpt-4o",  -- override for this task only
})
```

Model resolution priority: agent `model` > task `model` > router purpose > crew default.

## SSE Event Streaming

Monitor crew execution in real-time via Server-Sent Events:

```bash
# Start a run (returns immediately)
curl -X POST http://localhost:3000/flows/simple/run
# {"run_id":"abc-123","status":"started","events_url":"/flows/simple/events/abc-123"}

# Subscribe to real-time events
curl -N http://localhost:3000/flows/simple/events/abc-123
```

Event types:

| Event | Description |
|-------|-------------|
| `phase_start` | New execution phase begins |
| `task_assigned` | Task assigned to an agent |
| `task_completed` | Task finished successfully |
| `task_failed` | Task failed with error |
| `task_skipped` | Task skipped (condition/dependency) |
| `tool_call` | Agent invoked a tool |
| `log` | Lua `log()` call |
| `run_complete` | Crew run finished |

## Token Usage & Prompt Caching

Track API costs and leverage OpenAI prompt caching:

```lua
local crew = Crew.new({
    prompt_cache_key = "my-project-v1",  -- consistent routing for cache hits
    prompt_cache_retention = "24h",       -- extended caching on supported models
})

local results = crew:run()

-- Token usage available per task
for _, r in ipairs(results) do
    if r.token_usage then
        print(r.task .. ": " .. r.token_usage.total_tokens .. " tokens ("
              .. r.token_usage.cached_tokens .. " cached)")
    end
end
```

Inspect aggregated token usage via CLI:
```bash
ironcrew inspect <run_id> -p .
# Tokens: 1542 total (1024 cached)
```

## Custom Tools

```lua
-- tools/summarize.lua
return {
    name = "summarize",
    description = "Summarize text to a target length",
    parameters = {
        text = { type = "string", description = "Text to summarize", required = true },
        max_words = { type = "number", description = "Target word count" },
    },
    execute = function(args)
        -- Pure Lua logic, or use built-in helpers:
        -- fs.read(path), fs.write(path, content)
        -- env("VAR_NAME")
        return args.text:sub(1, (args.max_words or 100) * 5)
    end,
}
```

## Lua Globals

Available in all Lua contexts:

| Function | Description |
|----------|-------------|
| `env(name)` | Read environment variable |
| `uuid4()` | Generate UUID v4 |
| `now_rfc3339()` | Current time as RFC3339 |
| `now_unix_ms()` | Unix epoch in milliseconds |
| `json_parse(str)` | Parse JSON string to Lua table |
| `json_stringify(value)` | Serialize Lua value to JSON |
| `base64_encode(str)` | Base64 encode |
| `base64_decode(str)` | Base64 decode |
| `log(level, msg...)` | Structured logging (trace/debug/info/warn/error) |
| `regex.match(pat, text)` | Regex match (returns bool) |
| `regex.find(pat, text)` | Find first match |
| `regex.find_all(pat, text)` | Find all matches |
| `regex.captures(pat, text)` | Capture groups |
| `regex.replace(pat, text, repl)` | Replace first |
| `regex.replace_all(pat, text, repl)` | Replace all |
| `regex.split(pat, text)` | Split by pattern |
| `validate_json(str, schema)` | Validate JSON against a JSON Schema |

## Built-in Tools

| Tool | Description |
|------|-------------|
| `file_read` | Read file contents |
| `file_read_glob` | Read multiple files by glob pattern (e.g., `input/*.md`) |
| `file_write` | Write to file (path validation, extension whitelist) |
| `web_scrape` | Fetch URL and extract text |
| `shell` | Execute shell commands (opt-in, disabled by default) |
| `http_request` | HTTP client (GET/POST/PUT/DELETE/PATCH, auth, headers) |
| `hash` | Compute MD5, SHA256, SHA512 |
| `template_render` | Render Tera templates with JSON data |
| `validate_schema` | Validate JSON data against a JSON Schema |

## CLI

```bash
ironcrew init <name>          # Scaffold a new project
ironcrew run [path]           # Run a crew
ironcrew validate [path]      # Validate without executing
ironcrew list [path]          # List agents, tools, entrypoint
ironcrew nodes                # List built-in tools
ironcrew runs -p <path>       # List past runs
ironcrew inspect <id> -p .    # Inspect a run
ironcrew clean -p . --keep 10 # Purge old runs
ironcrew serve --port 3000    # Start REST API server
```

## REST API

```bash
ironcrew serve --flows-dir ./flows --port 3000
```

| Method | Path | Description |
|--------|------|-------------|
| GET | `/health` | Health check |
| POST | `/flows/{flow}/run` | Execute a crew (async, returns `run_id`) |
| GET | `/flows/{flow}/events/{run_id}` | SSE event stream for real-time monitoring |
| GET | `/flows/{flow}/runs` | List runs |
| GET | `/flows/{flow}/runs/{id}` | Get run details |
| DELETE | `/flows/{flow}/runs/{id}` | Delete a run |
| GET | `/flows/{flow}/validate` | Validate a flow |
| GET | `/flows/{flow}/agents` | List agents |
| GET | `/nodes` | List built-in tools |

## Using Other Providers

Any OpenAI-compatible API works by setting `base_url` and `api_key`:

```lua
-- Groq
local crew = Crew.new({
    model = "llama-3.3-70b-versatile",
    base_url = env("GROQ_API_URL"),
    api_key = env("GROQ_API_KEY"),
})

-- Ollama (local)
local crew = Crew.new({
    model = "llama3",
    base_url = "http://localhost:11434/v1",
})
```

## Examples

See the [`examples/`](examples/) directory:

| Example | Description |
|---------|-------------|
| `simple` | Single agent, basic task |
| `research-crew` | Multi-agent with dependencies and interpolation |
| `json-output` | JSON Schema structured output + file write |
| `groq-json` | Using Groq provider with JSON output |
| `parallel` | Independent tasks running concurrently |
| `conditional-crew` | Conditional tasks and error recovery |
| `collaborative` | Multi-agent debate with synthesis |
| `memory` | Shared memory system |
| `foreach` | Iterating over lists |
| `streaming` | Real-time LLM output |
| `subworkflow` | Nested crew composition |
| `model-router` | Purpose-based model routing |

## License

MIT
