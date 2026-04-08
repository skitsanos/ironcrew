# Crews

A crew is the central orchestration unit in IronCrew. It groups agents, tasks,
memory, and messaging into a single runnable workflow defined in Lua.

## Creating a Crew

```lua
local crew = Crew.new({
    goal            = "Analyze customer feedback and produce a report",
    provider        = "openai",               -- "openai" | "anthropic" | "openai-responses"
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
| `provider`               | string   | `"openai"`         | LLM provider: `"openai"`, `"anthropic"`, or `"openai-responses"` |
| `thinking_budget`        | number   | `nil`              | (Anthropic only) tokens allocated for extended thinking |
| `server_tools`           | table    | `{}`               | (Anthropic/Responses) server-side tools: `{"web_search"}`, `{"code_execution"}`, `{"file_search"}`, `{"code_interpreter"}` |
| `web_search_max_uses`    | number   | `nil`              | (Anthropic) max web search calls per task |
| `reasoning_effort`       | string   | `nil`              | (openai-responses) `"low"`, `"medium"`, `"high"` |
| `reasoning_summary`      | string   | `nil`              | (openai-responses) `"auto"`, `"concise"`, `"detailed"` |
| `web_search_context_size`| string   | `nil`              | (openai-responses) `"low"`, `"medium"`, `"high"` |
| `file_search_vector_store_ids` | table | `{}`            | (openai-responses) vector store IDs for file_search |
| `file_search_max_results`| number   | `nil`              | (openai-responses) max file_search results |
| `model`                  | string   | `"gpt-4.1-mini"`    | Default model for task execution |
| `base_url`               | string   | env `OPENAI_BASE_URL` | API endpoint (supports Gemini, Groq, etc.) |
| `api_key`                | string   | env `OPENAI_API_KEY`  | API key; auto-resolved from provider-specific env vars |
| `stream`                 | bool     | `false`            | Stream LLM responses token-by-token |
| `max_concurrent`         | number   | `10`               | Maximum tasks to run in parallel per phase. Overrides `IRONCREW_DEFAULT_MAX_CONCURRENT` env var |
| `memory`                 | string   | `"ephemeral"`      | `"ephemeral"` or `"persistent"` |
| `max_memory_items`       | number   | `500`              | Maximum items before LRU eviction |
| `max_memory_tokens`      | number   | `50000`            | Estimated token budget for memory |
| `prompt_cache_key`       | string   | `nil`              | Provider-side prompt cache identifier |
| `prompt_cache_retention` | string   | `nil`              | Cache retention hint (e.g. `"1h"`) |
| `models`                 | table    | `{}`               | Model router mapping (purpose -> model) |

---

## Project Defaults: `config.lua`

If a `config.lua` file exists at the project root (alongside `crew.lua`), it is
loaded automatically before `crew.lua` runs. It must return a table of default
settings — any field set there becomes a default for `Crew.new()`.

```lua
-- config.lua
return {
    provider = "anthropic",
    model = "claude-haiku-4-5-20251001",
    max_concurrent = 4,
    memory = "ephemeral",
    models = {
        task_execution = "claude-haiku-4-5-20251001",
        collaboration_synthesis = "claude-sonnet-4-5-20250929",
    },
}
```

```lua
-- crew.lua — only the workflow logic
local crew = Crew.new({
    goal = "Analyze a topic",
    -- provider, model, max_concurrent, memory, models inherited from config.lua
})
```

**Merge semantics:**

- **Shallow merge** — fields explicitly set in `Crew.new()` always win
- **No deep merge** — if both files define `models`, the user's `models` table
  fully replaces the config.lua one
- **All Crew.new() options supported** — `provider`, `model`, `base_url`,
  `max_concurrent`, `memory`, `max_memory_items`, `max_memory_tokens`, `stream`,
  `models`, `prompt_cache_key`, `prompt_cache_retention`, `thinking_budget`,
  `server_tools`, `web_search_max_uses`, `reasoning_effort`, `reasoning_summary`,
  `web_search_context_size`, `file_search_vector_store_ids`,
  `file_search_max_results`
- **Lua-powered** — config.lua runs in the same sandbox as crew.lua, so it can
  call `env()`, `now_rfc3339()`, etc. (sensitive env vars are blocked, same as
  crew.lua)

This keeps `crew.lua` focused on the workflow (goal, agents, tasks) while
provider/model/limits move to a single project-wide file. Useful for switching
providers between dev and prod by swapping `config.lua` only.

See [`examples/config-lua/`](../examples/config-lua/) for a working example.

---

## Conversation Mode

A `Conversation` is a stateful, multi-turn chat with an agent that maintains
its own message history across calls — different from a `Task`, which is
single-shot. Useful for stateful dialogues, agent testing, or interactive
workflows inside a Lua script.

Create a conversation bound to a crew (it inherits the crew's provider, model,
and tool registry):

```lua
local conv = crew:conversation({
    agent = "tutor",                          -- agent name (must be added to crew)
    -- OR: agent = Agent.new({...})           -- inline agent

    model = "claude-haiku-4-5-20251001",      -- optional override
    system_prompt = "You are a Rust tutor.",  -- optional override (else from agent)
    max_history = 20,                         -- optional cap on stored messages
    stream = true,                            -- optional, stream replies to stderr
})

-- Simple turn — returns just the reply text
local reply = conv:send("What is ownership in Rust?")

-- Full response with metadata (content + reasoning + length)
local response = conv:ask("Show me an example")
print(response.content)
print(response.reasoning)  -- present when using reasoning-capable providers
print(response.length)     -- total messages in history

-- History inspection
local history = conv:history()  -- table of {role, content, tool_call_id?}
local count = conv:length()
local agent_name = conv:agent_name()

-- Reset (clears all messages, keeps system prompt)
conv:reset()
```

**What's supported:**

- Multi-turn message history with shallow per-conversation isolation
- Tool calling — uses the crew's tool registry, full tool-call loop with timeout
- Streaming to stderr with dim reasoning (same model as task streaming)
- Reasoning capture from Anthropic, OpenAI Responses, DeepSeek, Kimi
- History cap (`max_history`) — oldest messages are trimmed first; system prompt is always preserved
- Provider/model/system_prompt overrides per conversation

**Limitations (current):**

- Single-agent only (use `crew:dialog({})` below for two-agent conversations)
- No cross-run persistence — conversations live for the duration of one `crew:run()` script

**SSE events:** Conversations emit `conversation_started`, `conversation_turn`,
and `conversation_thinking` events through the EventBus. REST API subscribers
on `/flows/{flow}/events/{run_id}` see them in real time alongside task events.
Each event includes a stable `conversation_id` so clients can group multiple
conversations within a single run. See [REST API](rest-api.md#sse-events) for
the full event schema.

See [`examples/conversation/`](../examples/conversation/) for a working example.

---

## Agent Dialog (Multi-Agent)

Two or more agents take turns in **round-robin** order with **perspective-flipped**
message histories — each agent sees its own past turns as `assistant` messages
and other participants' turns as `user` messages prefixed with the speaker's name.

### Two-agent dialog

```lua
local debate = crew:dialog({
    agent_a = "bull",                  -- agent name (or inline Agent.new())
    agent_b = "bear",
    starter = "Should we buy NVDA?",
    max_turns = 4,                     -- total turns combined (2 each here)
    starting_speaker = "a",            -- "a" (default), "b", or an agent name
    stream = true,                     -- prefix output with [agent_name] on stderr
    max_history = 30,                  -- optional cap on retained turns
})
```

### Multi-party dialog (3+ agents)

```lua
local roundtable = crew:dialog({
    agents = { "optimist", "pessimist", "realist" },  -- 2 or more agents
    starter = "Should we ship feature X this quarter?",
    max_turns = 6,                                     -- 2 rounds of 3 agents
    starting_speaker = "realist",                      -- by name
})
```

The `agents` array form supports any number of agents (≥ 2). Turns are taken
in round-robin order starting from `starting_speaker` (which accepts an agent
name or a positional letter `"a"`, `"b"`, `"c"`, ...). When `max_turns` is
omitted, it defaults to `2 * agents.len()` (two rounds each).

### Methods (same for both forms)

```lua
-- Run the entire dialog and return the transcript
local transcript = debate:run()
-- transcript = { {index=0, speaker="a", agent="bull", content="...", reasoning="..."}, ... }

-- Or step through interactively
local turn = debate:next_turn()           -- runs one turn, returns {index, speaker, agent, content, reasoning}
local count = debate:turn_count()         -- completed turns
local active = debate:current_speaker()   -- "a", "b", "c", ... or nil if finished
local active_name = debate:current_agent() -- agent name (or nil if finished)
local participants = debate:agents()       -- list of agent names
debate:reset()                             -- clear transcript and rewind
```

In SSE events and turn objects, `speaker` is always a positional letter
(`"a"`, `"b"`, `"c"`, ..., up to `"z"`) and `agent` is always the agent name.
Both fields are present so SSE consumers can use whichever is more useful.

**How perspective-flipping works:**

For each agent's turn, the engine builds a fresh message list from that
agent's viewpoint:
- **System** = that agent's `system_prompt`
- **Starter** → `role: "user"` (the kickoff prompt)
- **Their own previous turns** → `role: "assistant"`
- **Opponent's previous turns** → `role: "user"`, prefixed with `[opponent_name]:`

This way, each agent has a coherent first-person view of the dialog without
maintaining separate histories.

**The debate + moderator pattern:**

The most useful application is a **debate followed by a moderator synthesis**.
Two adversarial agents argue from committed positions, then a third agent
reads the transcript and produces a structured decision with explicit
falsification criteria. This turns "two LLMs talking" into "actionable output".

```lua
-- 1. Bull and Bear debate
local debate = crew:dialog({
    agent_a = "bull",
    agent_b = "bear",
    starter = data_summary .. "\nDebate the buy decision.",
    max_turns = 6,
})
local transcript = debate:run()

-- 2. Moderator synthesizes via a Conversation
local moderator = crew:conversation({ agent = "moderator" })
local synthesis = moderator:send(format_transcript(transcript))
-- The moderator agent has response_format = json_schema for structured output
```

The moderator agent uses `response_format = { type = "json_schema", ... }` to
return structured output (recommendation, confidence, agreed facts, key
disagreements, invalidation criteria).

This pattern generalizes well beyond stock analysis:

| Domain | Agent A | Agent B | Moderator output |
|--------|---------|---------|------------------|
| Investment | Bull | Bear | Buy / hold / sell + invalidation |
| Code review | "Ship it" advocate | Technical critic | Approve / changes / reject |
| Architecture | Microservices | Monolith | Decision + tradeoffs |
| Hiring | Hire advocate | Pass advocate | Hire / pass + signals |
| Product | Build now | Wait/pivot | Ship / hold + risks |

See [`examples/stock-debate/`](../examples/stock-debate/) for a complete
implementation: live data fetching from Yahoo Finance, two committed analyst
personas (each required to provide an INVALIDATION level per turn), and a
moderator that produces structured JSON synthesis.

**Other use cases:**
- Devil's advocate review of a proposal
- Two specialists discussing a problem from different angles
- Agent personality testing across many turns

**Limitations:**

- Two agents only (multi-party round-robin or moderator-driven dialog is future work)
- No early termination via Lua callback (only `max_turns` is supported)

**SSE events:** Dialogs emit `dialog_started`, `dialog_turn`,
`dialog_thinking`, and `dialog_completed` events through the EventBus. REST API
subscribers on `/flows/{flow}/events/{run_id}` see them in real time. Each
event includes a stable `dialog_id` and `turn_index`. See
[REST API](rest-api.md#sse-events) for the full event schema.

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
