# Agents

Agents are your AI specialists - each with a focused role, capabilities, and personality. You tell them *what* they're good at; IronCrew figures out *when* to use them.

> **Note:** Agent auto-selection is a simple word-overlap heuristic (capability, tool-keyword, and goal matching — see `AgentSelector` in `src/engine/agent.rs`), **not** an LLM-driven decision. See [Agent Selection Heuristics](#agent-selection-heuristics) below.

## Defining Agents

There are two ways to define agents: inline in `crew.lua` or as declarative files.

### Inline Definition

Use `Agent.new({...})` followed by `crew:add_agent()`:

```lua
crew:add_agent(Agent.new({
    name = "assistant",
    goal = "Answer programming questions clearly and concisely",
    capabilities = {"programming", "explanation"},
    temperature = 0.5,
}))
```

### Declarative File

Place a Lua file in the `agents/` directory that returns a table:

```lua
-- agents/extractor.lua
return {
    name = "extractor",
    goal = "Extract structured data from text into JSON format",
    system_prompt = "You are a data extraction specialist.",
    capabilities = {"extraction", "analysis", "json"},
    tools = {"file_write"},
    temperature = 0.1,
    response_format = {
        type = "json_schema",
        name = "company_analysis",
        schema = { ... },
    },
}
```

Agents loaded from files are auto-injected into every `Crew.new()` call. You do
not need to call `crew:add_agent()` for file-based agents.

## Agent Fields

| Field             | Type              | Required | Default                            | Description                                           |
|-------------------|-------------------|----------|------------------------------------|-------------------------------------------------------|
| `name`            | string            | yes      | --                                 | Unique identifier for the agent                       |
| `goal`            | string            | yes      | --                                 | What this agent aims to accomplish                     |
| `system_prompt`   | string            | no       | `"You are {name}. Your goal: {goal}"` | Custom system message sent to the LLM              |
| `capabilities`    | list of strings   | no       | `{}`                               | Keywords used for agent selection heuristics           |
| `tools`           | list of strings   | no       | `{}`                               | Names of tools this agent can invoke                  |
| `temperature`     | number            | no       | nil (provider default)             | LLM sampling temperature                              |
| `max_tokens`      | integer           | no       | nil (provider default)             | Maximum tokens in LLM response                        |
| `model`           | string            | no       | nil (uses crew default)            | Per-agent model override (highest priority)            |
| `expected_output` | string            | no       | nil                                | Description of what this agent should produce          |
| `response_format` | table             | no       | nil                                | Controls LLM output format (see below)                |
| `before_task`     | function          | no       | nil                                | Hook called before each task execution (see below)    |
| `after_task`      | function          | no       | nil                                | Hook called after each task execution (see below)     |

## Response Format

The `response_format` field controls how the LLM structures its output.
Three types are supported:

- **`text`** (default) -- Standard freeform text response.
- **`json_object`** -- Forces the LLM to return valid JSON (unstructured).
- **`json_schema`** -- Forces the LLM to return JSON conforming to a specific schema.
  Requires `name` and `schema` fields:

```lua
response_format = {
    type = "json_schema",
    name = "company_analysis",
    schema = {
        type = "object",
        properties = {
            companies = {
                type = "array",
                items = {
                    type = "object",
                    properties = {
                        name = { type = "string" },
                        industry = { type = "string" },
                        market_position = {
                            type = "string",
                            enum = {"leader", "challenger", "follower", "niche"},
                        },
                    },
                    required = {"name", "industry", "market_position"},
                    additionalProperties = false,
                },
            },
            summary = { type = "string" },
        },
        required = {"companies", "summary"},
        additionalProperties = false,
    },
}
```

This is particularly useful for data extraction pipelines where downstream tasks
expect a specific JSON structure.

## Agent Selection Heuristics

When a task does not specify `agent = "name"`, the engine auto-selects the best
agent by scoring each one against the task:

| Component        | Weight | Logic                                                                  |
|------------------|--------|------------------------------------------------------------------------|
| Capability match | 0.4    | Fraction of agent capabilities found as words in the task description  |
| Tool match       | 0.3    | Whether agent tools match tool keywords (`scrape`, `file`, `write`, etc.) in the description |
| Goal alignment   | 0.3    | Word overlap between agent goal and task description                   |

The score is `0.4 * capability + 0.3 * tool + 0.3 * goal`. On ties, the earlier-defined
agent wins.

To bypass auto-selection, assign an agent explicitly:

```lua
crew:add_task({
    name = "save_report",
    description = "Save the report to disk",
    agent = "reporter",   -- skip heuristics, use this agent
})
```

### Agents in Conversations and Dialogs

Beyond tasks, agents can also drive **stateful conversations** and
**agent-to-agent dialogs**. They are referenced by name from the same crew:

```lua
-- Single-agent multi-turn chat
local conv = crew:conversation({ agent = "tutor" })
local reply = conv:send("Explain ownership in Rust")

-- Two-agent debate (perspective-flipped)
local debate = crew:dialog({
    agents = { "bull", "bear" },
    starter = "Should we buy NVDA?",
    max_turns = 6,
})
```

Agents can also be passed inline as `Agent.new({...})` tables in both modes.
The agent's `system_prompt`, `temperature`, `model`, `tools`, and
`response_format` all carry over. See [Crews](crews.md#conversation-mode) for
the full Conversation and Dialog API.

**Tool-calling in conversations.** Tool-calling works inside `crew:conversation()`
tool-call loops — agents that declare `tools = { ... }` can invoke those tools
during any turn. This includes the built-in tools (see [Tools](tools.md) for the
built-in tool names), custom Lua tools under `tools/*.lua`, and MCP client tools
exposed as `mcp__<server>__<tool>` when the crew has `mcp_servers` configured.

**Image input.** `conv:send(msg, { images = {...} })` accepts an `images` list
where each entry is a file path (relative to the project directory), a URL
(`http(s)://...`), or a data URI. Added in 2.11.0. See
[Crews](crews.md#conversation-mode) for the full spec.

## Per-Agent Model Override

Each agent can specify a `model` that overrides the crew default. This is the
highest-priority model setting (see [Architecture](architecture.md) for the full
resolution chain):

```lua
crew:add_agent(Agent.new({
    name = "deep_thinker",
    goal = "Perform complex reasoning tasks",
    model = "gpt-4o",      -- uses gpt-4o even if crew default is gpt-4.1-mini
    temperature = 0.2,
}))
```

## Task Hooks

Agents can define `before_task` and `after_task` callback functions that run
around every task the agent executes. Hooks are useful for logging, metrics,
input preprocessing, and output postprocessing.

```lua
crew:add_agent(Agent.new({
    name = "researcher",
    goal = "Research topics thoroughly",
    before_task = function(task_name, task_description)
        log("info", "Starting: " .. task_name)
        return task_description  -- return modified description, or nil for no change
    end,
    after_task = function(task_name, output, success)
        log("info", "Done: " .. task_name .. " (" .. (success and "ok" or "fail") .. ")")
        return output  -- return modified output, or nil for no change
    end,
}))
```

### `before_task(task_name, task_description)`

Called before the agent sends its prompt to the LLM. Receives the task name and
interpolated description. Return a string to replace the description, or `nil`
to keep it unchanged.

### `after_task(task_name, output, success)`

Called after the LLM returns its response. Receives the task name, the raw LLM
output, and a boolean indicating success. Return a string to replace the output,
or `nil` to keep it unchanged.

### Notes

- Hooks run in an isolated Lua VM per invocation (no access to the crew's
  globals or memory).
- Hook errors are logged as warnings and do **not** fail the task -- the
  original description or output is used instead.
- Hooks are stored as Lua bytecode on the `Crew`, so they work across all
  execution modes: standard tasks, foreach tasks, and retry loops.
- Hooks do **not** run for error handler tasks or collaborative task synthesis
  calls.

## Best Practices

- **Name agents by role**, not capability: `"researcher"`, `"editor"`, `"analyst"` --
  not `"gpt4_agent"` or `"fast_agent"`.

- **Set capabilities that match task vocabulary.** The selection heuristic does
  word-level matching, so `capabilities = {"analysis", "data"}` will match tasks
  that mention those words.

- **Use `system_prompt` for persona and constraints.** The default prompt is generic;
  a custom system prompt lets you define tone, format rules, or domain expertise.

- **Assign tools explicitly.** Only tools listed in the agent's `tools` field are
  available to that agent. This prevents unwanted tool use and keeps the LLM prompt
  focused.

- **Use low temperature for structured output** (`0.1`) and higher for creative
  tasks (`0.7+`).

- **Use per-agent models for cost optimization.** Route complex reasoning to capable
  models and simple tasks to cheaper ones.
