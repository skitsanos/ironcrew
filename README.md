# IronCrew

A compiled Rust runtime for Lua-defined AI agent crews. Define agents, tasks, and orchestration logic in Lua — execute with a single native binary.

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

## Key Features

- **Lua scripting** — agents, tasks, tools, and orchestration defined in Lua
- **Parallel execution** — independent tasks run concurrently within topological phases
- **Provider-agnostic** — OpenAI, Gemini, Groq, Ollama, or any OpenAI-compatible API
- **Structured output** — JSON Schema `response_format` for validated LLM responses
- **9 built-in tools** — file I/O, HTTP, hashing, templates, schema validation
- **Memory & MessageBus** — shared state and agent-to-agent communication
- **Collaborative tasks** — multi-agent discussions with automatic synthesis
- **REST API + SSE** — run crews via HTTP with real-time event streaming
- **Single binary** — no runtime dependencies, Lua vendored and compiled in

## Quick Start

```bash
# Build
cargo build --release

# Create a new project
ironcrew init my-crew
cd my-crew

# Configure
echo "OPENAI_API_KEY=sk-..." > .env

# Run
ironcrew run .
```

## Documentation

| Guide | Description |
|-------|-------------|
| [Architecture](docs/architecture.md) | How IronCrew works — layers, execution model, project structure |
| [Agents](docs/agents.md) | Defining agents, capabilities, response formats, model overrides |
| [Tasks](docs/tasks.md) | Task options, dependencies, conditions, retries, foreach, collaborative |
| [Crews](docs/crews.md) | Crew configuration, memory, messaging, model router, prompt caching |
| [Tools](docs/tools.md) | Built-in tools, custom Lua tools, Lua globals and HTTP namespace |
| [CLI Reference](docs/cli.md) | All commands — run, validate, list, init, serve, inspect, clean |
| [REST API](docs/rest-api.md) | Endpoints, SSE events, input parameters, Docker deployment |
| [Providers](docs/providers.md) | OpenAI, Gemini, Groq, Ollama — configuration and tips |
| [Best Practices](docs/best-practices.md) | Prompt design, error handling, performance, security |

## Examples

See [`examples/`](examples/) for working demos:

`simple` · `research-crew` · `json-output` · `gemini` · `parallel` · `collaborative` · `memory` · `foreach` · `streaming` · `subworkflow` · `model-router` · `conditional-crew` · `groq-json`

## License

MIT
