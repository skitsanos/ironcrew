# IronCrew

**Build AI agent teams that work together.** IronCrew is a lightweight, high-performance framework for orchestrating multi-agent AI workflows. Write your agents, tasks, and logic in Lua — IronCrew handles parallel execution, tool calling, memory, and inter-agent communication in a single self-contained binary.

Works with OpenAI (Chat Completions + Responses API), Anthropic Claude (native), Google Gemini, Groq, Kimi K2.5, DeepSeek, xAI/Grok, Ollama, and any OpenAI-compatible API. Supports reasoning/thinking capture across providers. No Python, no Node.js, no Docker required — just one binary and your Lua scripts.

```lua
local crew = Crew.new({
    goal = "Research and summarize a topic",
    provider = "openai",
    model = env("OPENAI_MODEL") or "gpt-4.1-mini",
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
- **Project defaults via `config.lua`** — set provider, model, limits, and routing once per project; `crew.lua` stays focused on workflow logic
- **Conversation & Dialog modes** — stateful multi-turn chat with one agent (`crew:conversation({})`) or perspective-flipped multi-agent dialogs (`crew:dialog({})`) for two-agent debates or N-agent roundtables
- **Parallel execution** — independent tasks run concurrently within topological phases
- **Three provider types** — OpenAI Chat Completions, Anthropic native Messages API, OpenAI Responses API (also works with Gemini, Groq, Kimi, DeepSeek, xAI, Ollama via OpenAI compat)
- **Reasoning/thinking support** — captures chain-of-thought from Anthropic, DeepSeek, Kimi, and OpenAI Responses API; streams dim to stderr and persists to run records
- **Server-side tools** — built-in `web_search`, `code_execution`, `file_search`, `code_interpreter` via Anthropic and OpenAI Responses
- **Structured output** — JSON Schema `response_format` for validated LLM responses
- **9 built-in tools** — file I/O, HTTP, hashing, templates, schema validation
- **Memory & MessageBus** — shared state and agent-to-agent communication
- **Collaborative tasks** — multi-agent discussions with automatic synthesis
- **REST API + SSE** — run crews via HTTP with real-time event streaming
- **Production-hardened** — CORS, SSRF protection, graceful shutdown, rate limiting, request/response size limits
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
| [Chat & Conversations](docs/chat.md) | Phase 1 HITL — `ironcrew chat` REPL and HTTP conversation endpoints |
| [REST API](docs/rest-api.md) | Endpoints, SSE events, input parameters, Docker deployment |
| [HTTP Scaling](docs/http-scaling.md) | Capacity planning, session limits, SSE/proxy tuning, horizontal scaling |
| [Storage](docs/storage.md) | Storage backends — JSON files, SQLite, configuration, schema |
| [Providers](docs/providers.md) | OpenAI, Anthropic, OpenAI Responses, Gemini, Groq, Kimi, DeepSeek, xAI, Ollama — configuration, reasoning, server-side tools |
| [Cloud Deployment](docs/cloud-deployment.md) | Kubernetes, OpenShift, Railway — graceful shutdown, resource limits, security posture |
| [Best Practices](docs/best-practices.md) | Prompt design, error handling, performance, security |

## Examples

See [`examples/`](examples/) for working demos:

**Features:** `simple` · `research-crew` · `json-output` · `parallel` · `collaborative` · `memory` · `foreach` · `streaming` · `subworkflow` · `model-router` · `conditional-crew` · `http-api` · `batch-processing` · `config-lua` · `conversation` · `stock-debate` · `roundtable` · `moderator-dialog`

**Providers:** [`examples/providers/`](examples/providers/) contains 12 reference files covering every supported provider — OpenAI Chat, OpenAI Responses (basic, reasoning, web_search), Anthropic (basic, web_search, extended thinking), Gemini, Groq, Kimi K2.5, Kimi K2-thinking, and DeepSeek Reasoner.

**Anthropic:** [`examples/anthropic/`](examples/anthropic/) — native provider with extended thinking and server-side web_search.

**OpenAI Responses:** [`examples/responses/`](examples/responses/) — reasoning effort, streaming, built-in web search.

## License

MIT
