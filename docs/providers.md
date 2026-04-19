# LLM Providers

IronCrew supports three provider types:

1. **`openai`** — OpenAI Chat Completions API (and any OpenAI-compatible endpoint: Gemini, Groq, Kimi, DeepSeek, Ollama, Azure, OpenRouter)
2. **`anthropic`** — Native Anthropic Messages API with extended thinking, server-side tools, and prompt caching
3. **`openai-responses`** — OpenAI Responses API with first-class reasoning, built-in server-side tools, and cleaner streaming (OpenAI, Azure, xAI/Grok, OpenRouter)

Complete working examples for every provider are in [`examples/providers/`](../examples/providers/).

## Default Configuration

By default, IronCrew connects to the OpenAI API:

| Environment Variable | Default                     | Description   |
| -------------------- | --------------------------- | ------------- |
| `OPENAI_API_KEY`     | (required)                  | API key       |
| `OPENAI_BASE_URL`    | `https://api.openai.com/v1` | API base URL  |
| `OPENAI_MODEL`       | `gpt-4.1-mini`              | Default model |

Set these in a `.env` file in your project directory or export them in your shell.

## OpenAI Chat Completions (`provider = "openai"`)

The default provider. Also works with any OpenAI-compatible endpoint.

### OpenAI

No extra configuration needed beyond setting `OPENAI_API_KEY`.

```lua
local crew = Crew.new({
    goal = "My crew",
    provider = "openai",
    model = "gpt-4.1-mini",
})
```

### Google Gemini

Gemini exposes an OpenAI-compatible endpoint. Set `GEMINI_API_KEY` in your
environment — IronCrew auto-detects it when the base URL contains
`generativelanguage.googleapis.com` or `gemini`.

```lua
local crew = Crew.new({
    goal = "My crew",
    provider = "openai",
    model = "gemini-2.5-flash",
    base_url = "https://generativelanguage.googleapis.com/v1beta/openai",
})
```

Available models include `gemini-2.5-flash` and `gemini-2.5-pro`.
Gemini supports JSON Schema structured output via `response_format` on agents.
IronCrew handles Gemini-specific quirks automatically (array-wrapped error
responses, tool call arguments returned as objects instead of strings).

> **Gemini 3 preview caveat:** the `gemini-3-flash-preview` and `gemini-3-pro`
> endpoints emit `thought_signature` tokens that Gemini requires the client to
> echo back on tool-call follow-up turns. The OpenAI-compat bridge does not
> currently thread those signatures through, so Gemini 3 preview rejects any
> follow-up turn after a tool call. Stick to `gemini-2.5-flash` /
> `gemini-2.5-pro` for flows that use tools until the provider bridge learns
> to preserve thought signatures.

### Groq

Set `GROQ_API_KEY`. Auto-detected when base URL contains `groq.com`.

```lua
local crew = Crew.new({
    goal = "My crew",
    provider = "openai",
    model = "llama-3.3-70b-versatile",
    base_url = "https://api.groq.com/openai/v1",
})
```

### Kimi / Moonshot AI

Set `MOONSHOT_API_KEY`. Auto-detected when base URL contains `moonshot.ai` or
`moonshot.cn`. Kimi returns `reasoning_content` in responses which IronCrew
captures automatically into the `reasoning` field.

```lua
local crew = Crew.new({
    goal = "My crew",
    provider = "openai",
    model = "kimi-k2",
    base_url = "https://api.moonshot.ai/v1",
})
```

Available models: `kimi-k2`, `kimi-k2-thinking`, `kimi-k2-thinking-turbo`,
`moonshot-v1-8k`, `moonshot-v1-32k`, `moonshot-v1-128k`. Check the Moonshot
docs for the currently supported IDs.

### DeepSeek

Set `DEEPSEEK_API_KEY`. Auto-detected when base URL contains `deepseek.com`.
The `deepseek-reasoner` model returns `reasoning_content` which IronCrew
captures into the `reasoning` field.

```lua
local crew = Crew.new({
    goal = "My crew",
    provider = "openai",
    model = "deepseek-reasoner",
    base_url = "https://api.deepseek.com/v1",
})
```

### Ollama (Local)

Run models locally with no API key required.

```lua
local crew = Crew.new({
    goal = "My crew",
    provider = "openai",
    model = "llama3.2",
    base_url = "http://localhost:11434/v1",
    api_key = "ollama",
})
```

### Azure OpenAI

Use your Azure deployment endpoint as the base URL. Omit `api_key` — the Lua
sandbox's default `*_API_KEY` blocklist prevents `env("AZURE_OPENAI_API_KEY")`
from resolving, so the key must come from the Rust side.

```lua
local crew = Crew.new({
    goal = "My crew",
    provider = "openai",
    model = "gpt-4.1-mini",
    base_url = "https://YOUR-RESOURCE.openai.azure.com/openai/deployments/YOUR-DEPLOYMENT/v1",
})
```

Azure is not covered by the URL-based key auto-detection table below, so the
key falls back to `OPENAI_API_KEY`. In practice you have two options:

1. Export your Azure key as `OPENAI_API_KEY` before running `ironcrew`.
2. Relax the sandbox blocklist (see [docs/sandbox.md](sandbox.md)) so the Lua
   `env("AZURE_OPENAI_API_KEY")` call is permitted, then pass `api_key`
   explicitly.

## Anthropic Native (`provider = "anthropic"`)

IronCrew has a native Anthropic Messages API provider that unlocks features
unavailable via the OpenAI compat shim: **server-side web_search**,
**extended thinking**, prompt caching via `cache_control`, and block-based
streaming. Set `ANTHROPIC_API_KEY` in your environment.

### Basic usage

```lua
local crew = Crew.new({
    goal = "My crew",
    provider = "anthropic",
    model = "claude-haiku-4-5",
})
```

### Server-side tools

Anthropic executes `web_search` and `code_execution` on its own servers — no
custom tool or HTTP calls needed.

```lua
local crew = Crew.new({
    goal = "Research crew",
    provider = "anthropic",
    model = "claude-haiku-4-5",
    server_tools = { "web_search" },
    web_search_max_uses = 3,
})
```

Supported server tools:

| Tool | Config field | Description |
|------|-------------|-------------|
| `"web_search"` | `web_search_max_uses` (optional) | Server-side web search with cited sources |
| `"code_execution"` | — | Sandboxed Python execution |

### Extended thinking

```lua
local crew = Crew.new({
    goal = "Reasoning crew",
    provider = "anthropic",
    model = "claude-sonnet-4-5",
    thinking_budget = 5000,     -- tokens allocated for internal reasoning
    stream = true,              -- watch reasoning unfold dim on stderr
})
```

Thinking blocks are:
- **Streamed dim on stderr** during execution (visually distinct from output)
- **Persisted to the run record** under `task_results[].reasoning`
- **Emitted as `task_thinking` SSE events** for API subscribers

Available models: `claude-haiku-4-5`, `claude-sonnet-4-5`.

## OpenAI Responses API (`provider = "openai-responses"`)

The Responses API is OpenAI's newer endpoint with first-class reasoning items,
built-in server-side tools, and cleaner streaming semantics. Also supported by
**Azure OpenAI**, **xAI/Grok**, and **OpenRouter**.

### Basic usage

```lua
local crew = Crew.new({
    goal = "My crew",
    provider = "openai-responses",
    model = "gpt-4.1-mini",
})
```

### Reasoning

```lua
local crew = Crew.new({
    goal = "Reasoning crew",
    provider = "openai-responses",
    model = "gpt-4.1-mini",
    reasoning_effort = "medium",      -- "low" | "medium" | "high"
    reasoning_summary = "auto",       -- "auto" | "concise" | "detailed"
    stream = true,
})
```

Reasoning summaries are streamed dim to stderr and persisted to the run record.

### Built-in server-side tools

```lua
local crew = Crew.new({
    goal = "Research crew",
    provider = "openai-responses",
    model = "gpt-4.1-mini",
    server_tools = { "web_search", "file_search", "code_interpreter" },
    web_search_context_size = "medium",           -- "low" | "medium" | "high"
    file_search_vector_store_ids = { "vs_abc" },  -- required for file_search
    file_search_max_results = 10,
})
```

Supported server tools:

| Tool | Config | Description |
|------|--------|-------------|
| `"web_search"` | `web_search_context_size` | Built-in web search with citations |
| `"file_search"` | `file_search_vector_store_ids`, `file_search_max_results` | Semantic search over uploaded documents |
| `"code_interpreter"` | — | Python sandbox with file generation |

### xAI / Grok

```lua
local crew = Crew.new({
    goal = "My crew",
    provider = "openai-responses",
    model = "grok-4",
    base_url = "https://api.x.ai/v1",
})
```

Set `XAI_API_KEY`. Auto-detected when base URL contains `x.ai`. IronCrew
automatically falls back to a system-role message in `input` since Grok does
not support the `instructions` parameter.

## API Key Auto-Resolution

Key resolution depends on the `provider` value.

### `provider = "openai"`

1. Explicit `api_key` in the Crew config (note: Lua `env()` blocks `*_API_KEY`
   by default, so this is usually not set — env var auto-detection handles it).
2. Provider-specific env var based on the `base_url`:

| URL contains                                     | Env var              |
|--------------------------------------------------|----------------------|
| `generativelanguage.googleapis.com` or `gemini`  | `GEMINI_API_KEY`     |
| `groq.com`                                       | `GROQ_API_KEY`       |
| `anthropic.com`                                  | `ANTHROPIC_API_KEY`  |
| `moonshot.ai` or `moonshot.cn`                   | `MOONSHOT_API_KEY`   |
| `deepseek.com`                                   | `DEEPSEEK_API_KEY`   |
| `x.ai`                                           | `XAI_API_KEY`        |
| `openrouter.ai`                                  | `OPENROUTER_API_KEY` |

3. Fallback to `OPENAI_API_KEY`.

### `provider = "openai-responses"`

The Responses provider currently only special-cases xAI:

| URL contains | Env var       |
|--------------|---------------|
| `x.ai`       | `XAI_API_KEY` |

Everything else falls back to `OPENAI_API_KEY`. OpenRouter requests may still
be sent through the Responses path if OpenRouter itself accepts them, but
**URL-based key auto-detection is not wired** for OpenRouter on this provider
— export the key as `OPENAI_API_KEY` or set `api_key` explicitly.

### `provider = "anthropic"`

The key is always resolved from `ANTHROPIC_API_KEY`. No URL matching is
performed.

## Reasoning & Thinking Support

IronCrew captures reasoning/thinking output from all compatible providers into
a unified interface:

| Provider | Source | Config |
|----------|--------|--------|
| Anthropic | `thinking` content blocks | `thinking_budget = N` |
| OpenAI Responses | `reasoning` output items | `reasoning_effort = "medium"` |
| DeepSeek Reasoner | `reasoning_content` field | (automatic) |
| Kimi / K2-thinking | `reasoning_content` field | (automatic) |
| Moonshot | `reasoning_content` field | (automatic) |

**Where reasoning appears:**

- **Stderr:** During streaming, reasoning deltas appear in dim color, visually
  distinct from regular output
- **Run record:** `task_results[].reasoning` field, persisted to the store
  (JSON/SQLite/PostgreSQL) and visible via `ironcrew inspect`
- **SSE events:** `task_thinking` event type with `{task, agent, content}` payload
- **Lua interpolation:** `${results.task_name.reasoning}` (available but rarely
  useful — agents should use `output` for chained reasoning)

## Model Router

The model router lets you assign different models to different purposes within
the same crew, optimizing cost and performance.

```lua
local crew = Crew.new({
    goal = "Cost-optimized crew",
    provider = "openai",
    model = "gpt-4.1-mini",           -- default fallback
    models = {
        task_execution = "gpt-4.1-mini",
        collaboration = "gpt-4.1-mini",
        collaboration_synthesis = "gpt-4.1",
    },
})
```

Supported routing purposes:

| Purpose                    | When used                                  |
|----------------------------|--------------------------------------------|
| `task_execution`           | Main task execution (default purpose)      |
| `tool_synthesis`           | Synthesizing tool outputs back to text     |
| `final_response`           | Final crew goal summary                    |
| `collaboration`            | Collaborative task discussion turns        |
| `collaboration_synthesis`  | Synthesizing collaborative results         |

Resolution order: route for purpose → default model → crew model.

Individual agents and tasks can also override the model with a `model` field.

## Token Usage and Prompt Caching

Every task result includes token usage: `prompt_tokens`, `completion_tokens`,
`total_tokens`, and `cached_tokens`. Run records aggregate these across all tasks.

For providers that support prompt caching, enable it at the crew level:

```lua
local crew = Crew.new({
    goal = "My crew",
    prompt_cache_key = "my-cache-key",
    prompt_cache_retention = "1h",
})
```

Anthropic's prompt caching uses `cache_control` blocks — enabled automatically
when `prompt_cache_key` is set on the crew.

## Tips

- Use `gpt-4.1-mini`, `gemini-2.5-flash`, or `claude-haiku-4-5` for simple
  tasks. Reserve stronger models for tasks requiring deep reasoning.
- Set model overrides at the task level when a single task needs more capability
  than the rest of the crew.
- For reasoning-heavy tasks, use `provider = "openai-responses"` with
  `reasoning_effort = "medium"` or `provider = "anthropic"` with
  `thinking_budget`. Both capture the reasoning for later inspection.
- For research tasks, use server-side `web_search` — no custom HTTP calls, no
  SSRF concerns, and responses include proper citations.
- For local development, Ollama avoids API costs entirely. Switch providers in
  production by changing only the `.env` file.
- All providers communicate over HTTPS. The HTTP client has a 120-second timeout
  per request. SSRF protection blocks private IPs (override with
  `IRONCREW_ALLOW_PRIVATE_IPS=1`).

## See also: MCP servers

Model capability is only half the picture — crews can also attach
**Model Context Protocol (MCP) servers** to expose external tools to every
agent. Pass `mcp_servers = {...}` to `Crew.new({...})` with either a stdio
spawn spec or an HTTP Streamable URL, and the registered tools show up
alongside built-ins. See the MCP section of [docs/crews.md](crews.md) for
the full config schema, transport details, and examples.
