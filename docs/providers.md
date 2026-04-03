# LLM Providers

IronCrew uses a provider-agnostic architecture built on the OpenAI-compatible
chat completions API. Any service that implements the `/v1/chat/completions`
endpoint can be used as a backend.

## Default Configuration

By default, IronCrew connects to the OpenAI API:

| Environment Variable | Default                        | Description         |
|----------------------|--------------------------------|---------------------|
| `OPENAI_API_KEY`     | (required)                     | API key             |
| `OPENAI_BASE_URL`    | `https://api.openai.com/v1`    | API base URL        |
| `OPENAI_MODEL`       | `gpt-4o-mini`                  | Default model       |

Set these in a `.env` file in your project directory or export them in your shell.

## Supported Providers

### OpenAI (Default)

No extra configuration needed beyond setting `OPENAI_API_KEY`.

```lua
local crew = Crew.new({
    goal = "My crew",
    provider = "openai",
    model = "gpt-4o-mini",
})
```

### Google Gemini

Gemini exposes an OpenAI-compatible endpoint. Set `GEMINI_API_KEY` in your
environment -- IronCrew auto-detects it when the base URL contains
`generativelanguage.googleapis.com` or `gemini`.

```lua
local crew = Crew.new({
    goal = "My crew",
    provider = "openai",
    model = "gemini-3-flash-preview",
    base_url = "https://generativelanguage.googleapis.com/v1beta/openai",
    api_key = env("GEMINI_API_KEY"),
})
```

Available models include:

- `gemini-2.5-flash` -- fast, cost-effective
- `gemini-3-flash-preview` -- next-generation preview
- `gemini-3.1-flash-lite-preview` -- lightweight preview

Gemini supports JSON Schema structured output via `response_format` on agents.
IronCrew handles Gemini-specific quirks automatically, including array-wrapped
error responses and tool call arguments returned as objects instead of strings.

### Groq

Set `GROQ_API_KEY` in your environment. IronCrew auto-detects it when the
base URL contains `groq.com`.

```lua
local crew = Crew.new({
    goal = "My crew",
    provider = "openai",
    model = "llama-3.3-70b-versatile",
    base_url = "https://api.groq.com/openai/v1",
    api_key = env("GROQ_API_KEY"),
})
```

### Ollama (Local)

Run models locally with no API key required. Point `base_url` to your Ollama
instance and pass an empty string or dummy value for `api_key`.

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

Use your Azure deployment endpoint as the base URL.

```lua
local crew = Crew.new({
    goal = "My crew",
    provider = "openai",
    model = "gpt-4o",
    base_url = "https://YOUR-RESOURCE.openai.azure.com/openai/deployments/YOUR-DEPLOYMENT/v1",
    api_key = env("AZURE_OPENAI_API_KEY"),
})
```

## API Key Resolution

When `Crew.new()` includes a `base_url`, IronCrew resolves the API key in this
order:

1. Explicit `api_key` in the Crew config
2. Provider-specific environment variable based on the base URL:
   - `generativelanguage.googleapis.com` or `gemini` -> `GEMINI_API_KEY`
   - `groq.com` -> `GROQ_API_KEY`
   - `anthropic.com` -> `ANTHROPIC_API_KEY`
3. Fallback to `OPENAI_API_KEY`

This means you can set `GEMINI_API_KEY` in your `.env` and omit `api_key` from
the Crew config -- it will be picked up automatically.

## Model Router

The model router lets you assign different models to different purposes within
the same crew, optimizing cost and performance.

```lua
local crew = Crew.new({
    goal = "Cost-optimized crew",
    provider = "openai",
    model = "gpt-4o-mini",           -- default fallback
    models = {
        task_execution = "gpt-4o-mini",
        collaboration = "gpt-4o-mini",
        collaboration_synthesis = "gpt-4o",
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

Resolution order: route for purpose -> default model -> crew model.

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

## Tips

- Use `gpt-4o-mini` or `gemini-2.5-flash` for simple extraction and routing tasks.
  Reserve `gpt-4o` or larger models for tasks requiring deep reasoning.
- Set model overrides at the task level when a single task needs more capability
  than the rest of the crew.
- Use the model router to keep collaboration turns cheap while using a stronger
  model for the final synthesis.
- For local development, Ollama avoids API costs entirely. Switch to a cloud
  provider for production by changing only the `.env` file.
- All providers communicate over HTTPS. The HTTP client has a 120-second timeout
  per request.
