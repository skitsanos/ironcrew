# Crews

A crew is the central orchestration unit in IronCrew. It groups agents, tasks,
memory, and messaging into a single runnable workflow defined in Lua.

## Creating a Crew

```lua
local crew = Crew.new({
    goal            = "Analyze customer feedback and produce a report",
    provider        = "openai",               -- only "openai" supported (works with any OpenAI-compatible API)
    model           = "gpt-4.1-mini",          -- default model for all tasks
    base_url        = "https://api.openai.com/v1",  -- optional, overrides OPENAI_BASE_URL
    api_key         = env("OPENAI_API_KEY"),  -- optional, overrides OPENAI_API_KEY
    stream          = false,                  -- enable streaming output (default false)
    max_concurrent  = 4,                      -- max parallel tasks (default: IRONCREW_DEFAULT_MAX_CONCURRENT or 10)
    memory          = "ephemeral",            -- "ephemeral" (default) or "persistent"
    max_memory_items  = 500,                  -- eviction threshold (default 500)
    max_memory_tokens = 50000,                -- estimated token cap (default 50 000)
    prompt_cache_key       = "my-crew",       -- cache key sent to the provider
    prompt_cache_retention = "1h",            -- cache retention hint

    -- Model router (see below)
    models = {
        task_execution         = "gpt-4o",
        tool_synthesis         = "gpt-4.1-mini",
        final_response         = "gpt-4o",
        collaboration          = "gpt-4o",
        collaboration_synthesis = "gpt-4.1-mini",
    },
})
```

### Configuration Reference

| Key                      | Type     | Default            | Description |
|--------------------------|----------|--------------------|-------------|
| `goal`                   | string   | *required*         | High-level objective shown in the system prompt |
| `provider`               | string   | `"openai"`         | LLM provider (must be `"openai"`) |
| `model`                  | string   | `"gpt-4.1-mini"`    | Default model for task execution |
| `base_url`               | string   | env `OPENAI_BASE_URL` | API endpoint (supports Gemini, Groq, etc.) |
| `api_key`                | string   | env `OPENAI_API_KEY`  | API key; auto-resolved from provider-specific env vars |
| `stream`                 | bool     | `false`            | Stream LLM responses token-by-token |
| `max_concurrent`         | number   | `nil` (sequential) | Maximum tasks to run in parallel |
| `memory`                 | string   | `"ephemeral"`      | `"ephemeral"` or `"persistent"` |
| `max_memory_items`       | number   | `500`              | Maximum items before LRU eviction |
| `max_memory_tokens`      | number   | `50000`            | Estimated token budget for memory |
| `prompt_cache_key`       | string   | `nil`              | Provider-side prompt cache identifier |
| `prompt_cache_retention` | string   | `nil`              | Cache retention hint (e.g. `"1h"`) |
| `models`                 | table    | `{}`               | Model router mapping (purpose -> model) |

---

## Memory System

Every crew has a key-value memory store. Agents can read and write shared state
across tasks, enabling multi-step workflows where later tasks build on earlier
results.

### Basic Operations

```lua
crew:memory_set("summary", "The product received mixed reviews")
local val = crew:memory_get("summary")  -- returns the value, or nil
crew:memory_delete("summary")           -- returns true if key existed
crew:memory_clear()                     -- wipe all keys
```

### Extended Set (Tags and TTL)

```lua
crew:memory_set_ex("user_prefs", {theme = "dark"}, {
    tags   = {"user", "settings"},   -- tags for relevance scoring
    ttl_ms = 60000,                  -- auto-expire after 60 seconds
})
```

### Listing and Inspection

```lua
local keys = crew:memory_keys()   -- returns a table of all active keys
local stats = crew:memory_stats() -- { total_items, total_tokens }
```

### Persistent vs Ephemeral

| Mode          | Lifecycle | Storage |
|---------------|-----------|---------|
| `"ephemeral"` | Lost when the process exits | In-memory only |
| `"persistent"`| Survives across runs | `.ironcrew/memory.json` in the project directory |

Persistent memory is loaded on crew creation and saved automatically after
`crew:run()`. Expired items (TTL-based) are filtered out on load.

### Eviction

When the store exceeds `max_memory_items` or `max_memory_tokens`, the
least-recently-used items are evicted. The eviction score considers:

1. Access count (lower = evicted first)
2. Last update timestamp (older = evicted first)
3. Internal revision counter (lower = evicted first)

Expired items (past their TTL) are always removed before applying limits.

---

## MessageBus

The message bus allows agents to exchange messages during a run. Messages are
typed and queued per-agent.

### Sending Messages

```lua
-- Send a notification to a specific agent
crew:message_send("analyst", "writer", "Draft is ready for review")

-- Send with explicit type: "notification" (default), "request", or "broadcast"
crew:message_send("manager", "analyst", "Please re-check section 3", "request")

-- Broadcast to all agents
crew:message_send("manager", "*", "Deadline extended by 1 hour", "broadcast")
```

### Reading Messages

```lua
-- Consume all pending messages for an agent (removes them from the queue)
local msgs = crew:message_read("writer")
for _, msg in ipairs(msgs) do
    print(msg.from, msg.content, msg.type, msg.timestamp)
end
```

### Message History

```lua
-- Read-only log of all messages sent during this run
local history = crew:message_history()
for _, msg in ipairs(history) do
    print(msg.from .. " -> " .. msg.to .. ": " .. msg.content)
end
```

History is capped at the last 500 messages.

### Broadcast Delivery

Broadcasts (`to = "*"`) are delivered to all registered agent queues except the
sender. If sent before agents are registered, they are stored as pending and
delivered when each agent registers.

---

## Model Router

The model router lets you assign different models to different execution phases
without changing agent or task definitions.

```lua
local crew = Crew.new({
    goal  = "Multi-model workflow",
    model = "gpt-4.1-mini",         -- fallback for unrouted purposes
    models = {
        task_execution          = "gpt-4o",
        tool_synthesis          = "gpt-4.1-mini",
        final_response          = "gpt-4o",
        collaboration           = "gpt-4o",
        collaboration_synthesis = "gpt-4.1-mini",
    },
})
```

### Resolution Priority

When the engine selects a model for a task phase, it checks in order:

1. **Agent-level model** -- `model` field on the agent definition
2. **Task-level model** -- `model` field on the task definition
3. **Router mapping** -- the `models` table keyed by purpose
4. **Router default** -- if set via the router's internal default
5. **Crew default** -- the top-level `model` in `Crew.new()`

### Available Purposes

| Purpose                    | When Used |
|----------------------------|-----------|
| `task_execution`           | Main LLM call for a task |
| `tool_synthesis`           | Synthesizing tool call results back into text |
| `final_response`           | Generating the crew's final summary |
| `collaboration`            | Each discussion turn in a collaborative task |
| `collaboration_synthesis`  | Merging collaborative discussion into a result |

---

## Prompt Caching

For providers that support prompt caching (e.g., OpenAI), you can set a cache
key and retention hint at the crew level:

```lua
local crew = Crew.new({
    goal = "Cached workflow",
    prompt_cache_key       = "feedback-analysis-v2",
    prompt_cache_retention = "1h",
})
```

These values are passed through to the LLM provider.

---

## Token Usage Tracking

Each task result includes a `token_usage` table with `prompt_tokens`,
`completion_tokens`, `total_tokens`, and `cached_tokens`. Totals are persisted
in run history and visible via `ironcrew inspect`.

---

## Subworkflows

A crew can delegate to another Lua workflow file within the same project:

```lua
local result = crew:subworkflow("sub/analysis.lua", {
    input      = { data = "some input data" },
    output_key = "analysis_result",
})
```

### Parameters

| Key          | Type   | Description |
|--------------|--------|-------------|
| `input`      | table  | Passed as the `input` global in the subworkflow's Lua VM |
| `output_key` | string | If set, the return value is wrapped as `{[output_key] = result}` |

### Behavior

- The subworkflow runs in its own Lua VM with a fresh `Crew.new()` scope.
- It shares the parent's `Runtime` (tool registry, provider) but has its own
  crew, memory, and message bus.
- Agents in the subworkflow's `agents/` directory (relative to the script) are
  auto-loaded.
- The path must be relative and must not escape the project directory (no `..`,
  no absolute paths).
- The subworkflow script's return value is serialized through JSON and
  transferred back to the parent VM.
