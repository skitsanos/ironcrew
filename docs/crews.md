# Crews

A crew is the central orchestration unit in IronCrew. It groups agents, tasks,
memory, and messaging into a single runnable workflow defined in Lua.

## Creating a Crew

```lua
local crew = Crew.new({
    goal            = "Analyze customer feedback and produce a report",
    provider        = "openai",               -- "openai" | "anthropic" | "openai-responses"
    model           = "gpt-5.6-luna",          -- default model for all tasks
    base_url        = "https://api.openai.com/v1",  -- optional, overrides OPENAI_BASE_URL
    api_key         = env("OPENAI_API_KEY"),  -- optional, overrides OPENAI_API_KEY
    stream          = false,                  -- enable streaming output (default false)
    max_concurrent  = 4,                      -- max parallel tasks (default: IRONCREW_DEFAULT_MAX_CONCURRENT or 4)
    memory          = "ephemeral",            -- "ephemeral" (default) or "persistent"
    max_memory_items  = 500,                  -- eviction threshold (default 500)
    max_memory_tokens = 50000,                -- estimated token cap (default 50 000)
    prompt_cache_key       = "my-crew",       -- cache key sent to the provider
    prompt_cache_retention = "1h",            -- cache retention hint

    -- Model router (see below)
    models = {
        task_execution         = "gpt-4o",
        tool_synthesis         = "gpt-5.6-luna",
        final_response         = "gpt-4o",
        collaboration          = "gpt-4o",
        collaboration_synthesis = "gpt-5.6-luna",
    },
})
```

### Configuration Reference

| Key                      | Type     | Default            | Description |
|--------------------------|----------|--------------------|-------------|
| `goal`                   | string   | *required*         | Non-empty high-level objective shown in the system prompt; capped by `IRONCREW_CREW_GOAL_MAX_BYTES` (default 64 KiB) |
| `provider`               | string   | `"openai"`         | LLM provider: `"openai"`, `"anthropic"`, or `"openai-responses"`; maximum 128 bytes |
| `thinking_budget`        | number   | `nil`              | (Anthropic only) tokens allocated for extended thinking; `1..=1000000` |
| `server_tools`           | table    | `{}`               | (Anthropic/Responses) dense, duplicate-free server-side tool list; count capped by `IRONCREW_MAX_SERVER_TOOLS` |
| `web_search_max_uses`    | number   | `nil`              | (Anthropic) max web search calls per task; `1..=100` |
| `reasoning_effort`       | string   | `nil`              | (openai-responses) `"low"`, `"medium"`, `"high"` |
| `reasoning_summary`      | string   | `nil`              | (openai-responses) `"auto"`, `"concise"`, `"detailed"` |
| `web_search_context_size`| string   | `nil`              | (openai-responses) `"low"`, `"medium"`, `"high"` |
| `file_search_vector_store_ids` | table | `{}`            | (openai-responses) vector store IDs for file_search |
| `file_search_max_results`| number   | `nil`              | (openai-responses) max file_search results; `1..=1000` |
| `model`                  | string   | `"gpt-5.6-luna"`    | Non-empty default model for task execution; maximum 1024 bytes |
| `base_url`               | string   | env `OPENAI_BASE_URL` | HTTP(S) API endpoint, maximum 4096 bytes; userinfo, query strings, and fragments are rejected |
| `api_key`                | string   | env `OPENAI_API_KEY`  | API key, maximum 16 KiB; padding/control characters are rejected |
| `stream`                 | bool     | `false`            | Stream LLM responses token-by-token |
| `max_concurrent`         | number   | `4`                | Maximum tasks to run in parallel per phase. Overrides `IRONCREW_DEFAULT_MAX_CONCURRENT` env var |
| `memory`                 | string   | `"ephemeral"`      | `"ephemeral"` or `"persistent"` |
| `max_memory_items`       | number   | `500`              | Maximum items before LRU eviction |
| `max_memory_tokens`      | number   | `50000`            | Estimated token budget for memory |
| `prompt_cache_key`       | string   | `nil`              | Provider-side prompt cache identifier |
| `prompt_cache_retention` | string   | `nil`              | Cache retention hint (e.g. `"1h"`) |
| `models`                 | table    | `{}`               | Model router mapping (purpose -> model) |
| `mcp_servers`            | table    | `{}`               | External [MCP](#mcp-model-context-protocol-tool-servers) tool servers (stdio / HTTP) connected when tools are first finalized for a run or conversation |

---

`max_memory_items` and `max_memory_tokens` must be positive and cannot exceed
the operator policies `IRONCREW_MAX_MEMORY_ITEMS` (default 10000, hard 100000)
and `IRONCREW_MAX_MEMORY_TOKENS` (default 1000000, hard 10000000). Provider
configuration is also bounded: at most 16 `server_tools`, 32 vector-store IDs,
and 64 `models` routes by default, with hard ceilings of 64, 256, and 256.
Server-tool names, vector-store IDs, and model-route purposes are non-empty and
capped at 4096 bytes; model-route values use the 1024-byte model-name cap.
Lists must be dense arrays without duplicates. Invalid values fail crew
construction rather than being silently ignored.

**Unknown keys are rejected.** `Crew.new`, agent tables, and task tables each
accept a closed set of options. An unrecognized key fails construction with a
message naming the offending key and listing the supported ones. This is
deliberate: a silently ignored typo in `require_approval` would disable the
approval gate without any signal, and a typo in `depends_on` would drop a task
dependency. Options from a provider you are not using (for example
`thinking_budget` on OpenAI) are still accepted and simply unused.

---

## Project Defaults: `config.lua`

If a `config.lua` file exists at the project root (alongside `crew.lua`), it is
loaded automatically before `crew.lua` runs. It must return a table of default
settings — any field set there becomes a default for `Crew.new()`.

```lua
-- config.lua
return {
    provider = "anthropic",
    model = "claude-haiku-4-5",
    max_concurrent = 4,
    memory = "ephemeral",
    models = {
        task_execution = "claude-haiku-4-5",
        collaboration_synthesis = "claude-sonnet-4-5",
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

> **Guard top-level `crew:run()` when mixing chat and task execution.**
> IronCrew sets `IRONCREW_MODE` as a Lua global before `crew.lua` runs:
> `"run"` under `ironcrew run` and `"chat"` under `ironcrew chat` (and the
> HTTP conversation endpoints). If your script defines tasks and also
> exposes a conversational agent, wrap the bootstrapping call:
>
> ```lua
> if IRONCREW_MODE ~= "chat" then
>     crew:run()
> end
> ```
>
> This prevents chat-mode boot-up (REPL or HTTP `/start`) from triggering a
> full task execution just to instantiate the crew.

Create a conversation bound to a crew (it inherits the crew's provider, model,
and tool registry):

```lua
local conv = crew:conversation({
    agent = "tutor",                          -- agent name (must be added to crew)
    -- OR: agent = Agent.new({...})           -- inline agent

    model = "claude-haiku-4-5",      -- optional override
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

### Image input

Pass images to vision-capable models alongside text messages:

```lua
local reply = conv:send("Describe this image", {
    images = { "path/to/photo.jpg" }
})

-- Multiple images
local reply = conv:send("Compare these two logos", {
    images = { "logo-a.png", "logo-b.png" }
})

-- URL
local reply = conv:send("What's in this photo?", {
    images = { "https://example.com/photo.jpg" }
})
```

Both `send()` and `ask()` accept an optional second argument — a table
with an `images` key containing an array of file paths or URLs.

**Supported formats:** JPEG, PNG, GIF, WebP (up to 20 MB per image).

**Provider compatibility:** works with any vision-capable model — GPT-4o,
Gemini Flash, Claude (Anthropic), and OpenAI-compatible endpoints. Each
provider's image format is handled automatically.

Image paths are resolved relative to the project directory. URLs are
downloaded at send time.

See [`examples/vision/`](../examples/vision/) for a working example with
Gemini Flash.

### Cross-run persistence

By default a conversation is ephemeral — it exists for the lifetime of one
`crew:run()` and disappears when the process exits. Pass a stable `id` to
persist it and resume across separate `ironcrew run` invocations (or API
requests):

```lua
local chat = crew:conversation({
    id       = "support-ticket-4821",   -- any ASCII [a-zA-Z0-9._-]{1,128}
    agent    = "support_bot",
    -- autosave = false,                -- opt out of auto-save (default: true)
})

chat:send("I'm seeing 504s from the billing API")
-- State is saved after every completed turn.
```

When the same code runs again with the same `id`, IronCrew loads the prior
message history from the store only when its versioned execution identity still
matches. That identity binds the Lua source tree, selected Agent, resolved
model/system prompt, transcript limits, tool rounds, effective non-secret
provider endpoint/options, and resolved tool graph. API keys are excluded.
Omit `id` to get the pre-2.8 ephemeral behavior.

Direct Lua/CLI turns on a persistent PostgreSQL conversation fail closed.
Shared-store turns must use the HTTP `/messages` endpoint with an
`Idempotency-Key`, which acquires the durable incarnation/revision fence before
rehydrating or invoking the provider and tools. JSON and SQLite retain the
single-process direct `send()`/`ask()` behavior.

**Session methods:**

| Method                | Description |
|-----------------------|-------------|
| `conv:id()`           | The stable session id (user-supplied or auto-UUID) |
| `conv:is_persistent()`| `true` if `id` was supplied and the session is tied to the store |
| `conv:save()`         | Explicit save — useful when `autosave = false` |
| `conv:delete()`       | Remove the persisted record from the store |

Sessions are stored via the same `StateStore` backend as run history
(JSON, SQLite, or PostgreSQL). Records are keyed by `(flow_path, id)` so
different flows can reuse the same id. The JSON backend lays them out at
`<conversations_dir>/<flow>/<id>.json` (typically
`.ironcrew/conversations/<flow>/<id>.json`); SQL backends store the same
composite key in the `conversations` table. Legacy flat records from
before multi-flow isolation remain in `<conversations_dir>/<id>.json`
and are reachable only via global/admin lookups. See
[`examples/cross-run-persistence/`](../examples/cross-run-persistence/)
for a full walkthrough.

**Gotchas:** IDs are restricted to alphanumerics plus `-`, `_`, `.` (1-128
chars) to prevent path traversal and SQL oddness; violations fail loud at
the Lua layer. Saves carry an optimistic revision and a UUID incarnation.
Delete/recreate assigns a new incarnation so stale responses cannot cross that
boundary. Identity-less records from older versions remain available for
history export/delete but cannot resume; recreate them after exporting.

**SSE events:** Conversations emit `conversation_started`, `conversation_turn`,
and `conversation_thinking` events through the EventBus. REST API subscribers
on `/flows/{flow}/events/{run_id}` see them in real time alongside task events.
Each event includes a stable `conversation_id` so clients can group multiple
conversations within a single run. See [REST API](rest-api.md#sse-event-stream) for
the full event schema.

See [`examples/conversation/`](../examples/conversation/) for a basic example
and [`examples/cross-run-persistence/`](../examples/cross-run-persistence/)
for the persistence demo.

### HTTP Conversation Endpoints

The same `crew:conversation({})` primitive is exposed over HTTP by
`ironcrew serve` as six endpoints. Sessions are created explicitly with
`POST /start`; only one turn may mutate an id at a time (overlap returns
`409`, never a queued request); and state persists through the same
`StateStore` used by `ironcrew chat`.

With PostgreSQL, `/messages` always requires an `Idempotency-Key` and can
cold-rehydrate on either replica. Conversation SSE is not shared: PostgreSQL
returns `409` for `/events`, while JSON/SQLite provide process-local streams
without `Last-Event-ID` replay.

| Method | Path                                                | Purpose                           |
| ------ | --------------------------------------------------- | --------------------------------- |
| POST   | `/flows/{flow}/conversations/{id}/start`            | Create or re-open a chat session  |
| POST   | `/flows/{flow}/conversations/{id}/messages`         | Send a user turn, wait for reply  |
| GET    | `/flows/{flow}/conversations/{id}/history`          | Read the stored transcript        |
| GET    | `/flows/{flow}/conversations/{id}/events`           | SSE stream for the session        |
| DELETE | `/flows/{flow}/conversations/{id}`                  | Drop handle + delete record       |
| GET    | `/flows/{flow}/conversations`                       | Paginated list (filtered by flow) |

See [REST API: Conversations](rest-api.md#conversations-phase-1-human-in-the-loop)
for request/response shapes and a worked curl session. Production clients
should also follow the [Idempotency-Key retry contract](rest-api.md#safe-retries-with-idempotency-key).

### Chat & Conversation Env Vars

| Variable | Default | Description |
|----------|---------|-------------|
| `IRONCREW_MAX_ACTIVE_CONVERSATIONS` | `8` | Hard cap on simultaneously-active in-memory chat handles across the server. Breaches return `503` |
| `IRONCREW_CHAT_SESSION_IDLE_SECS`   | `1800` | Idle timeout before an in-memory chat handle is evicted (its record stays on disk) |
| `IRONCREW_CONVERSATIONS_DEFAULT_LIMIT` | `20`  | Default page size for `GET /flows/{flow}/conversations` |
| `IRONCREW_CONVERSATIONS_MAX_LIMIT`  | `100` | Hard cap on the `limit` query parameter for the same endpoint |
| `IRONCREW_CONVERSATION_MAX_HISTORY` | `50`  | Default retained non-system messages for `crew:conversation({})`; explicit zero is rejected and the hard ceiling is 4096 |

---

## Agent Dialog (Multi-Agent)

Two or more agents take turns in **round-robin** order with **perspective-flipped**
message histories — each agent sees its own past turns as `assistant` messages
and other participants' turns as `user` messages prefixed with the speaker's name.

### Basic dialog

```lua
local debate = crew:dialog({
    agents = { "bull", "bear" },       -- two or more agents, in turn order
    starter = "Should we buy NVDA?",
    max_turns = 4,                     -- total turns combined (2 each here)
    starting_speaker = "bull",         -- agent name or positional letter
    stream = true,                     -- prefix output with [agent_name] on stderr
    max_history = 30,                  -- optional cap on retained turns
})
```

### Multi-party dialog (3+ agents)

```lua
local roundtable = crew:dialog({
    agents = { "optimist", "pessimist", "realist" },
    starter = "Should we ship feature X this quarter?",
    max_turns = 6,                                    -- 2 rounds of 3 agents
    starting_speaker = "realist",                     -- by name
})
```

The `agents` array supports any number of participants (≥ 2). Turns are taken
in round-robin order starting from `starting_speaker`, which accepts either an
agent name or a positional letter `"a"`, `"b"`, `"c"`, ... When `max_turns` is
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

Dialog construction enforces bounded policy even when a flow supplies larger
values:

| Variable | Default | Description |
|---|---:|---|
| `IRONCREW_DIALOG_MAX_HISTORY` | `100` | Default retained turns; explicit zero is rejected, hard ceiling 4095 |
| `IRONCREW_DIALOG_MAX_TURNS` | `1000` | Maximum accepted total turns; hard ceiling 10000 |
| `IRONCREW_DIALOG_MAX_PARTICIPANTS` | `16` | Maximum accepted participant count; hard ceiling 64 |
| `IRONCREW_CHAT_HISTORY_MAX_BYTES` | `33554432` | Shared estimated prompt/transcript byte budget; hard ceiling 256 MiB |

In SSE events and turn objects, `speaker` is a positional label (`"a"` through
`"z"`, then `"agent_26"`, `"agent_27"`, ...) and `agent` is always the agent
name. Both fields are present so SSE consumers can use whichever is more
useful.

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
    agents = { "bull", "bear" },
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

### Moderator-driven speaker selection

Instead of round-robin, pass a `turn_selector` Lua function that decides
who speaks next. The callback receives the transcript so far and the list of
agent names, and returns the name of the next speaker.

```lua
local moderator = crew:conversation({ agent = "facilitator" })

local dialog = crew:dialog({
    agents = { "product", "engineering", "customer_success" },
    starter = "Should we launch next Tuesday?",
    max_turns = 6,
    turn_selector = function(transcript, agents)
        if #transcript == 0 then return "product" end
        -- Ask the facilitator agent who should speak next
        return moderator:send("Who should speak next? " .. format(transcript))
    end,
})
local transcript = dialog:run()
```

The callback is called via `call_async`, so it can call async methods like
`moderator:send()`. You can also use the simpler `dialog:next_turn_from(name)`
method for fully manual control in a loop:

```lua
for i = 1, 6 do
    local next = moderator:send("Who next?")
    local turn = dialog:next_turn_from(next)
    if not turn then break end
end
```

See [`examples/moderator-dialog/`](../examples/moderator-dialog/) for a
complete implementation with an LLM-driven facilitator.

### Custom early termination

Dialogs keep running until they reach `max_turns`, but you often want to
stop earlier when some condition is met (consensus detected, a threshold
crossed, a stop keyword found). Pass a `should_stop` callback:

```lua
local dialog = crew:dialog({
    agents = { "alice", "bob" },
    starter = "Negotiate a fair price for the clock",
    max_turns = 20,  -- generous safety cap
    should_stop = function(last_turn, transcript)
        -- last_turn: {index, speaker, agent, content, reasoning?}
        -- transcript: full array of completed turns
        if last_turn.content:find("AGREED") and #transcript >= 2 then
            return "consensus reached"  -- stop, reason stored + emitted
        end
        return false  -- continue
    end,
})

local transcript = dialog:run()
local reason = dialog:stop_reason()  -- "consensus reached" or nil
```

The callback fires after every turn (automatic or manual) and can return:

| Return value    | Effect |
|-----------------|--------|
| `nil` / `false` | Continue the dialog |
| `true`          | Stop; reason = `"custom_stop"` |
| `"reason"`      | Stop with that reason string |
| Anything else   | Usage error — surfaces as a validation failure |

Like `turn_selector`, the callback is invoked via `call_async`, so you can
use async methods inside it (e.g. ask another agent to judge whether the
debate has converged). The `max_turns` value still acts as a hard safety
ceiling — it bounds the worst case if the callback never returns a stop
signal.

**Querying state from Lua:**

| Method                   | Returns |
|--------------------------|---------|
| `dialog:stopped()`       | `true` if `should_stop` requested termination |
| `dialog:stop_reason()`   | The reason string, or `nil` for normal completion |

When the dialog stops early, the `dialog_completed` SSE event carries the
reason:

```json
{"type": "dialog_completed", "dialog_id": "...", "total_turns": 5, "stop_reason": "consensus reached"}
```

The `stop_reason` field is omitted for runs that terminate via `max_turns`
(backward-compatible with older clients). See
[`examples/dialog-early-stop/`](../examples/dialog-early-stop/) for a
full negotiation example.

### Cross-run persistence

Dialogs support the same `id`-keyed persistence as conversations. Supply
a stable `id` and IronCrew saves the transcript, `next_index`, and stop
state after every turn; re-opening the dialog with the same `id` on a
subsequent run resumes from exactly where it left off.

```lua
local debate = crew:dialog({
    id      = "ship-decision-q2",
    agents  = { "optimist", "pessimist" },
    starter = "Should we ship the billing rewrite this sprint?",
    max_turns = 6,
    -- autosave = false,   -- opt out of auto-save (default: true)
})

debate:run()   -- picks up from next_index if a prior record exists
```

On resume, the **agent list is validated against the stored record** —
if you save a dialog with `{ "alice", "bob" }` and then try to resume it
with `{ "alice", "carol" }`, the resume fails with a clear validation
error rather than silently mixing state.

**Session methods (same shape as conversation):**

| Method                  | Description |
|-------------------------|-------------|
| `dialog:id()`           | The stable dialog id (user-supplied or auto-UUID) |
| `dialog:is_persistent()`| `true` if the dialog is tied to the store |
| `dialog:save()`         | Explicit save (for `autosave = false`) |
| `dialog:delete()`       | Remove the persisted record |
| `dialog:turn_count()`   | Returns `next_index` — reflects prior runs too |

Dialogs share the same `(flow_path, id)` keying as conversations. The
JSON backend lays them out at `<dialogs_dir>/<flow>/<id>.json` (typically
`.ironcrew/dialogs/<flow>/<id>.json`); SQL backends store the composite
key in the `dialogs` table. Legacy flat records remain in
`<dialogs_dir>/<id>.json` and are visible only to global/admin lookups.
Dialog saves use the same revision conflict rule as conversations, so a stale
pod cannot overwrite a newer transcript.
The
[`examples/cross-run-persistence/`](../examples/cross-run-persistence/)
project demonstrates both a resumable conversation and a resumable dialog
in the same script.

**SSE events:** Dialogs emit `dialog_started`, `dialog_turn`,
`dialog_thinking`, and `dialog_completed` events through the EventBus. REST API
subscribers on `/flows/{flow}/events/{run_id}` see them in real time. Each
event includes a stable `dialog_id` and `turn_index`. See
[REST API](rest-api.md#sse-event-stream) for the full event schema.

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
`crew:run()`. Expired items (TTL-based) are filtered out on load. Saves are
serialized, written to a mode-`0600` temporary file, synced, and atomically
renamed so racing snapshots or a process interruption cannot expose a partial
JSON file.

Input and persistence budgets are independent from the LRU item/token policy:

| Variable | Default | Purpose |
|---|---:|---|
| `IRONCREW_MEMORY_MAX_KEY_BYTES` | `1024` | Key bytes |
| `IRONCREW_MEMORY_MAX_VALUE_BYTES` | `1048576` | Serialized value bytes |
| `IRONCREW_MEMORY_MAX_TAGS` | `32` | Tags per item |
| `IRONCREW_MEMORY_MAX_TAG_BYTES` | `256` | Bytes per tag |
| `IRONCREW_MEMORY_PERSIST_MAX_BYTES` | `16777216` | Loaded/saved snapshot bytes |
| `IRONCREW_MEMORY_CONTEXT_MAX_BYTES` | `65536` | Memory context injected into one prompt |
| `IRONCREW_MEMORY_QUERY_MAX_BYTES` | `16384` | Query bytes |

Zero or invalid values fall back to these bounded defaults.

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

History defaults to the last 500 messages and 4 MiB. One message is truncated
at 64 KiB; each agent queue defaults to 1000 messages/4 MiB, and broadcasts
sent before registration default to 500 messages/4 MiB. Configure the count
and byte policies with `IRONCREW_MESSAGEBUS_{MESSAGE_MAX_BYTES,QUEUE_DEPTH,QUEUE_MAX_BYTES,HISTORY_DEPTH,HISTORY_MAX_BYTES,PENDING_CAP,PENDING_MAX_BYTES}`.
Only queue-depth and pending-count caps accept zero as disabled; byte caps and
history depth remain bounded.

### Broadcast Delivery

Broadcasts (`to = "*"`) are delivered to all registered agent queues except the
sender. If sent before agents are registered, they are stored as pending and
delivered when each agent registers.

---

## Human-in-the-Loop: `ask_human`

`crew:ask_human(opts)` suspends the flow until a human answers — the mid-run
counterpart to conversations (which are human-driven from the start). Use it
for approval points, missing parameters, or any decision the flow shouldn't
make on its own.

```lua
-- Free-form answer
local region = crew:ask_human({ prompt = "Which region should this report cover?" })

-- Constrained choice with timeout + fallback
local decision = crew:ask_human({
    prompt    = "Ready to publish. Proceed?",
    choices   = { "publish", "hold" },
    timeout_s = 300,
    default   = "hold",
})

-- Structured answer: whatever JSON the caller posts comes back as a Lua value
local params = crew:ask_human({ prompt = "Adjust thresholds (JSON object expected)" })
print(params.max_items)
```

| Field | Type | Required | Meaning |
|-------|------|----------|---------|
| `prompt` | string | yes | Question shown to the human |
| `choices` | array of strings | no | Advisory choice list, surfaced to UIs for rendering buttons. Not enforced — free-text answers are accepted; validate in Lua if you need strict values. |
| `timeout_s` | integer | no | Per-question timeout. Default `IRONCREW_ASK_HUMAN_TIMEOUT` (600 s). |
| `default` | any | no | Returned on timeout instead of raising an error |

By default, one run may have 16 pending questions and the configured maximum
timeout is 3600 seconds; prompts, aggregate choices, and serialized answers are each
limited to 64 KiB; and a question may list at most 100 choices. These policies
are controlled by `IRONCREW_ASK_HUMAN_MAX_PENDING`,
`IRONCREW_ASK_HUMAN_MAX_TIMEOUT`, `IRONCREW_ASK_HUMAN_MAX_PROMPT_BYTES`,
`IRONCREW_ASK_HUMAN_MAX_CHOICES`, `IRONCREW_ASK_HUMAN_MAX_CHOICES_BYTES`, and
`IRONCREW_ASK_HUMAN_MAX_ANSWER_BYTES`.

Returns the answer as a Lua value. On timeout **without** a `default`, raises
`ask_human timed out after <n>s` — catch with `pcall` or let the task-retry
machinery treat it like any other failure.

### Where the answer comes from

- **Server mode** (`ironcrew serve`): the run suspends, the SSE stream emits
  `human_input_requested`, and the answer arrives via
  [`POST /flows/{flow}/answer/{run_id}`](rest-api.md#answer-a-question). A UI
  that missed the event lists pending questions with
  `GET /flows/{flow}/questions/{run_id}`. For an idempotency-keyed HTTP run,
  PostgreSQL plus the shared HITL encryption keyring lets either endpoint enter
  through any replica. PostgreSQL-backed run SSE can also replay through any
  replica; execution remains on the owner, PostgreSQL conversation SSE is
  unsupported, and JSON/SQLite conversation SSE remains process-local. See
  [cross-replica delivery](rest-api.md#cross-replica-delivery).
- **CLI mode** (`ironcrew run`): the prompt (and numbered choices) print to
  stderr and the answer is read from stdin. A bare number picks the matching
  choice. Non-TTY stdin (piped, CI) resolves as an immediate timeout, so
  unattended runs fall through to `default` or fail cleanly instead of
  hanging — the same flow works attended and unattended.

Parallel branches (`foreach_parallel`) may each ask concurrently; questions
are answered independently by `question_id`, capped at
`IRONCREW_ASK_HUMAN_MAX_PENDING` (default 16) and
`IRONCREW_ASK_HUMAN_MAX_PENDING_BYTES` (default 1 MiB of aggregate serialized
question metadata) per run.

Note on run status: the persisted run record flips to `waiting_for_input`
only when a run record exists at ask time (the record is created inside
`crew:run()`). For the common pattern — asking in flow code before or between
runs — the questions endpoint is the authoritative "waiting" signal.

### Letting agents ask (the `ask_human` tool)

`crew:ask_human()` is scripted by the flow author at fixed points. To let an
**agent decide mid-reasoning** that it needs the human, give it the built-in
[`ask_human` tool](tools.md#ask_human):

```lua
crew:add_agent({
    name = "analyst",
    goal = "analyze quarterly data",
    tools = { "ask_human" },
})
```

When the model calls the tool, the task suspends on the same per-run
transport — same SSE events, same `questions`/`answer` endpoints, same
terminal prompt in CLI mode. The human sees who's asking (`[analyst] …`).
Every named agent gets its own attributed question, so one crew may pause for
sequential or concurrent conversations with several agents. Answers are bound
to `question_id`; clients must never infer the recipient from prompt text or
submission order.
Two behaviors are specific to the agent path:

- **Human-wait time is excluded from the task timeout.** A task suspended on
  a question is observably waiting, not stuck, so `timeout_secs` doesn't
  tick while a question is pending (`IRONCREW_MAX_RUN_LIFETIME` still bounds
  the whole run).
- **Timeouts return a soft result**, not an error: the model gets a
  `[no answer]` message instructing it to proceed on its best judgment,
  so it doesn't retry into another full wait.

Delegated agents (`agent__<name>`) inherit the transport, so a sub-agent can
also pause to ask.

The v3 release gate executes this contract three ways: a real terminal-backed
`ironcrew run` with two named agents, a real `ironcrew serve` question/list/
answer/SSE round trip, and two independent server processes sharing disposable
PostgreSQL 15 storage and one HITL keyring. In the replica case the run remains
owned by one process while the other process lists and answers both agents'
questions; PostgreSQL coordinates encrypted delivery but does not migrate the
Lua execution.

### Approval gates (`require_approval`)

Gate specific tools behind a human sign-off — sandboxing controls what tools
*can* do, approval gates control what they *may* do per-invocation:

```lua
local crew = Crew.new({
    goal = "quarterly close",
    require_approval = { "http_request", "file_write", "agent__deployer", "mcp__git__*" },
})
```

Entries are exact tool names or prefix globs (trailing `*`); `"*"` gates
everything. Operators can enforce a policy without editing flows via
`IRONCREW_REQUIRE_APPROVAL` (comma-separated, same syntax) — the two lists
are unioned. Works from `config.lua` project defaults too. A crew accepts at
most `IRONCREW_MAX_APPROVAL_PATTERNS` entries (default 128, hard ceiling 1024),
and each non-empty pattern is capped at 512 bytes.

When a gated tool is called, the run suspends on an approval question
(`kind: "approval"` on SSE and the questions endpoint; a prompt in CLI
mode) showing the agent, the tool, and its **redacted** arguments
(sensitive-looking keys like `authorization`/`token`/`api_key` are masked;
the serialized form is capped at `IRONCREW_APPROVAL_ARGS_MAX_CHARS`,
default 600):

```
[approval] Agent 'analyst' wants to call http_request({"method":"POST",...}). Allow?
  1. allow    -- run this call
  2. always   -- run it, and stop asking for this tool this flow execution
  3. deny     -- refuse; the agent sees "denied by human operator"
```

**Fail closed:** timeout (`IRONCREW_APPROVAL_TIMEOUT`, default = the
ask_human default), a missing approval channel, or any answer that isn't an
exact allow token all **deny**. A free-text answer denies AND is forwarded
to the model as the reason — "no, use the cached data instead" becomes
agent steering.

The policy rides the tool registry, so it automatically covers built-ins,
MCP tools, custom Lua tools, and `agent__<name>` delegation (the delegation
itself can be gated, and delegated sub-agents inherit the caller's policy
and its "always" grants). `ask_human` itself is never gated. Human-wait
time is excluded from task timeouts, same as ask_human.

### Steering dialogs with ask_human

Dialog callbacks are ordinary Lua, so a human can arbitrate an agent-to-agent
dialog without any dedicated machinery:

```lua
local dialog = crew:dialog({
    agents = { "optimist", "skeptic" },
    max_turns = 12,
    should_stop = function(last_turn, transcript)
        -- Every 4 turns, let the human decide whether the debate continues.
        -- should_stop is invoked via call_async, so the suspension works.
        if #transcript % 4 == 0 then
            local verdict = crew:ask_human({
                prompt    = "Turn " .. last_turn.index .. ": continue the debate?",
                choices   = { "continue", "stop" },
                timeout_s = 120,
                default   = "continue",
            })
            if verdict == "stop" then
                return "stopped by human moderator"
            end
        end
        return false
    end,
})
```

### Environment Variables

| Variable | Default | Meaning |
|----------|---------|---------|
| `IRONCREW_ASK_HUMAN_TIMEOUT` | `600` | Default per-question timeout (seconds) when `timeout_s` is omitted |
| `IRONCREW_ASK_HUMAN_MAX_PENDING` | `16` | Per-run cap on simultaneously pending questions |

---

## Model Router

The model router lets you assign different models to different execution phases
without changing agent or task definitions.

```lua
local crew = Crew.new({
    goal  = "Multi-model workflow",
    model = "gpt-5.6-luna",         -- fallback for unrouted purposes
    models = {
        task_execution          = "gpt-4o",
        tool_synthesis          = "gpt-5.6-luna",
        final_response          = "gpt-4o",
        collaboration           = "gpt-4o",
        collaboration_synthesis = "gpt-5.6-luna",
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

### `run_flow(path, input)` — sandbox-level sub-flow

`run_flow` is a sandbox-level Lua global available in **every** VM
IronCrew creates — the top-level `crew.lua`, any `tools/*.lua` custom
tool, and the tool-call handlers invoked during a conversational turn. It
does not require a `Crew` instance.

```lua
local result = run_flow("subs/analyze.lua", { input = value })
```

**Signature**

| Arg     | Type   | Description |
|---------|--------|-------------|
| `path`  | string | Path to a Lua flow, resolved relative to the calling VM's project directory. Absolute paths, `..`, and anything that escapes the project root are rejected |
| `input` | table? | Optional table passed to the sub-flow as the `input` Lua global |

Returns whatever the sub-flow's final Lua expression yields. Tables
round-trip as tables, primitives as primitives, everything else collapses
to `nil`.

**Behavior**

- Runs in-process in a fresh `Crew.new` sandbox; JSON is the only
  transport between the caller and the sub-flow's VM
- Shares the parent's `Runtime` (tool registry, provider, MCP
  connections) but gets its own crew, memory, and message bus
- Nesting is capped by `IRONCREW_MAX_FLOW_DEPTH` (default `5`); exceeding
  the cap fails with a validation error
- Emits `log` events through the caller's EventBus:
  `run_flow: <path>` at start and `run_flow done: <path> (<ms>ms)` at
  completion

**When to use it over `crew:subworkflow()`**

`crew:subworkflow()` is still supported and keeps its `output_key` sugar
for wrapping the result into a single-field table. Prefer `run_flow`
when:

- You need sub-flow dispatch from a place that does not have a `crew`
  handle (custom tool Lua, a conversational agent's tool-call path)
- You want the raw return value without `output_key` wrapping
- You want the call to work identically inside nested flows

See [`examples/chat-http`](../examples/chat-http/) for a chat flow that
delegates to a sub-crew via a custom tool that calls `run_flow()`.

---

## MCP (Model Context Protocol) Tool Servers

IronCrew can connect to external [MCP](https://modelcontextprotocol.io/) servers and expose
their tools to agents. Supported transports are **stdio** (child process) and
**Streamable HTTP**. IronCrew requires MCP `2026-07-28`: it starts every
connection with `server/discover` and does not send `initialize`,
`notifications/initialized`, or fall back to an older lifecycle.

MCP is enabled by default (Cargo feature `mcp`). Build without it via `--no-default-features`.

### Quick start

```lua
local crew = Crew.new({
    goal = "Echo a message through a local MCP tool",
    provider = "openai",
    model    = "gpt-5.6-luna",

    mcp_servers = {
        -- Label must match ^[a-z][a-z0-9_-]{0,15}$
        local_tools = {
            transport = "stdio",
            -- Required if this server's tools are reachable from a persistent conversation.
            execution_identity = "ironcrew-mcp-2026-fixture-v1",
            command   = "python3",
            args      = { "examples/mcp/stdio-tools/server.py" },
        },
    },
})

crew:add_agent({
    name  = "analyst",
    role  = "MCP tool user",
    goal  = "Echo the requested text",
    -- Tools are exposed as  mcp__<server_label>__<tool_name>
    tools = { "mcp__local_tools__echo" },
})
```

### Transport options

#### Stdio

```lua
mcp_servers = {
    local_tools = {
        transport   = "stdio",
        execution_identity = "ironcrew-mcp-2026-fixture-v1",
        command     = "python3",        -- binary to spawn
        args        = { "examples/mcp/stdio-tools/server.py" },
        env         = { MY_VAR = "val" },  -- extra env vars for child (optional)
        inherit_env = false,            -- default: false (safer for cloud)
    },
}
```

**Security:** by default, child processes only inherit `PATH`, `HOME`, `USER`, `LANG`, and
`LC_*` variables. Your `OPENAI_API_KEY` and other secrets are **not** forwarded.
Set `inherit_env = true` to opt in to full inheritance (not recommended in production).

#### HTTP

```lua
mcp_servers = {
    myapi = {
        transport = "http",
        execution_identity = "catalog-api-v3",
        url       = "https://mcp.example.com/mcp",
        headers   = {
            authorization = "Bearer " .. env("MCP_API_TOKEN"),
        },
    },
}
```

HTTP URLs are validated against the SSRF filter (private IPs blocked by default).
Localhost is blocked unless `IRONCREW_MCP_ALLOW_LOCALHOST=1` is set.
Configure the Streamable HTTP POST endpoint (normally `/mcp`), not a legacy
SSE endpoint. IronCrew uses the sessionless `2026-07-28` lifecycle: every
request carries its protocol version, client identity, and capabilities, and
no `Mcp-Session-Id` is negotiated.

HTTP discovery also enforces final-revision `x-mcp-header` routing. IronCrew
supports statically reachable nested `properties` paths, promotes only
string/boolean/JavaScript-safe-integer arguments, and encodes unsafe header text
with the protocol Base64 sentinel. A tool is excluded when any annotation is
unreachable, ambiguous, duplicated, or attached to another type. On exact
`HeaderMismatch` code `-32020`, one complete paginated discovery refresh and
one call retry share the original deadline and attempt cap; any non-header tool
definition drift, a missing tool, or a second mismatch poisons the connection.

### Persistent-conversation execution identity

MCP discovery describes tool names and schemas, but it cannot prove that two
replicas are connected to behaviorally equivalent server code or data. If an
MCP tool is reachable from a persistent conversation's selected agent tool
graph, its server must set `execution_identity` to a stable, non-secret value
(maximum 4096 bytes, no control characters). IronCrew hashes the value and
binds that fingerprint, the discovered tool schema, server/tool names, and the
resolved dependency graph into the conversation definition. For HTTP tools the
compiled parameter-header plan and policy version are bound as well. The raw
identity is not persisted.

Change the value whenever the executable/image, API implementation, relevant
non-secret configuration, or data contract changes. Do not put bearer tokens,
API keys, header values, raw environment secrets, or hashes of guessable
secrets in it. For example, use a package/image digest plus a configuration
revision for stdio, or a deployment/API-contract revision for HTTP. An MCP
server without this field can still serve ordinary runs and ephemeral
conversations; persistent conversation construction fails closed if the
selected tool graph reaches it.

### Tool naming

Tools discovered from MCP servers are registered in the IronCrew tool registry as:

```
mcp__<server_label>__<tool_name>
```

Constraints:
- Server label: `^[a-z][a-z0-9_-]{0,15}$` (lowercase, max 16 chars)
- Composed name: ≤ 64 characters total

### Connection lifecycle

- MCP servers are **connected in parallel** when tools are first finalized for
  a run or conversation.
- Discovery is strict MCP `2026-07-28`. A server that rejects
  `server/discover` or omits that supported version fails the connection; there
  is no legacy initialization fallback. The server must declare its `tools`
  capability before IronCrew sends `tools/list`.
- Stdio MCP is supported on Unix, where IronCrew can own and kill the complete
  server process group. Windows deployments must use sessionless Streamable
  HTTP so timeout/abort cleanup does not leave unowned descendants.
- The connection is **cached** for the crew's lifetime — subsequent `crew:run()` calls
  reuse the same connections without reconnecting.
- On crew drop / server shutdown, connections are torn down gracefully: stdio process
  groups are killed and the directly owned child is reaped, while HTTP request/SSE paths close locally. See
  [cloud-deployment.md](cloud-deployment.md#graceful-shutdown) for tuning the drain
  window (`IRONCREW_SHUTDOWN_DRAIN_MS`) on Kubernetes / Railway.

### Multi-round tool calls

IronCrew supports bounded, state-only multi round-trip requests (MRTR). When a
tool returns `resultType = "input_required"` with a `requestState`, IronCrew
echoes that opaque string exactly and retries the same call. An empty string is
valid. The call deadline spans all attempts and backoff, and the total number
of wire attempts is capped by `IRONCREW_MCP_MAX_MRTR_ROUNDS`.

IronCrew advertises no optional client capabilities or extensions. Non-empty
`inputRequests` (including elicitation, sampling, or roots requests) and
`resultType = "task"` therefore fail closed rather than being fulfilled or
polled. An `input_required` result with neither a usable request nor
`requestState` is invalid.

Deadline, caller cancellation, or a protocol/capability violation poisons the
connection so no later call or discovery request can reuse it. Stdio poisoning
synchronously kills the process group; an independently owned supervisor reaps
the direct child, and explicit shutdown waits for confirmation. HTTP teardown
closes the local request, but it cannot undo or prove termination of remote
work already started.

### Security environment variables

| Variable | Default | Description |
|---|---|---|
| `IRONCREW_MCP_ALLOWED_COMMANDS` | (unset = allow all) | Comma-separated exact commands allowed for stdio. Present-but-empty refuses all commands. E.g. `"uvx,npx"`. |
| `IRONCREW_MCP_ALLOW_LOCALHOST` | `0` | Set to `1` to allow localhost/loopback HTTP URLs. |
| `IRONCREW_MCP_DISCOVERY_TIMEOUT_SECS` | `10` | Seconds to wait for `server/discover` and connection setup. The former handshake variable is ignored; there is no compatibility alias. |
| `IRONCREW_MCP_MAX_MRTR_ROUNDS` | `10` | Maximum total wire attempts for one state-only MRTR call (hard ceiling `32`). |
| `IRONCREW_MCP_MAX_REQUEST_STATE_BYTES` | `65536` | Maximum UTF-8 bytes in an echoed opaque `requestState` (hard ceiling `1048576`). |
| `IRONCREW_MCP_MAX_INBOUND_MESSAGE_BYTES` | `1048576` | Pre-JSON cap for one stdio line, HTTP JSON message, or SSE event (hard ceiling `16777216`). One transport chunk may temporarily exceed the cap but is rejected before copying into IronCrew-owned assembly/parser buffers. |
| `IRONCREW_MCP_TOOL_RESULT_MAX_BYTES` | `262144` (256 KB) | Maximum bytes returned per tool call. Oversized results are truncated with a `[truncated: N bytes omitted]` marker. |

### Examples

- `examples/mcp/stdio-tools/` — dependency-free MCP `2026-07-28` stdio example and fixture
- `examples/mcp/http-tools/` — strict `2026-07-28` Streamable HTTP example
