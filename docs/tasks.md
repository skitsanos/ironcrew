# Tasks

Tasks define what your crew actually does. Each task is a prompt sent to an agent, with options for dependencies, retries, conditions, error
handling, and more.

## Basic Task

```lua
crew:add_task({
    name = "answer",
    description = "Explain what ownership means in Rust in 2-3 sentences.",
    expected_output = "A clear, concise explanation of Rust ownership",
})
```

The `name` and `description` fields are required. Everything else is optional.

## Task Fields

| Field                | Type            | Required | Default     | Description                                              |
|----------------------|-----------------|----------|-------------|----------------------------------------------------------|
| `name`               | string          | yes      | --          | Unique identifier for the task                           |
| `description`        | string          | yes      | --          | Prompt sent to the LLM                                   |
| `agent`              | string          | no       | auto-select | Name of the agent to execute this task                   |
| `expected_output`    | string          | no       | nil         | Describes what the response should look like             |
| `context`            | string          | no       | nil         | Additional context injected into the prompt              |
| `depends_on`         | list of strings | no       | `{}`        | Task names this task depends on                          |
| `model`              | string          | no       | nil         | Per-task model override                                  |
| `max_retries`        | integer         | no       | 0           | Number of retry attempts on failure                      |
| `retry_backoff_secs` | number          | no       | 1.0         | Base backoff in seconds (doubles each attempt)           |
| `timeout_secs`       | integer         | no       | 300         | Maximum seconds before the task times out                |
| `condition`          | string          | no       | nil         | Lua expression; task runs only if truthy                 |
| `on_error`           | string          | no       | nil         | Name of an error handler task                            |
| `stream`             | boolean         | no       | false       | Stream LLM response to stderr in real-time               |
| `foreach`            | string          | no       | nil         | Key in results/memory to iterate over (JSON array)       |
| `foreach_as`         | string          | no       | `"item"`    | Variable name for the current iteration item             |
| `task_type`          | string          | no       | `"standard"` | Set to `"collaborative"` for multi-agent discussion     |
| `agents`             | list of strings | no       | `{}`        | Agent names for collaborative tasks (min 2)              |
| `max_turns`          | integer         | no       | 3           | Max conversation turns for collaborative tasks           |

## Dependencies and Topological Ordering

Tasks declare dependencies with `depends_on`. The engine groups tasks into phases
using Kahn's algorithm: tasks with no unmet dependencies form a phase and run
concurrently. The next phase begins after all tasks in the current phase complete.

```lua
-- Phase 0: task_a, task_b, task_c run in parallel
crew:add_task({ name = "task_a", description = "..." })
crew:add_task({ name = "task_b", description = "..." })
crew:add_task({ name = "task_c", description = "..." })

-- Phase 1: summary waits for all three
crew:add_task({
    name = "summary",
    description = "Compare the three languages...",
    depends_on = {"task_a", "task_b", "task_c"},
})
```

The engine validates the graph before execution:

- References to nonexistent tasks produce an error
- Duplicate task names produce an error
- Circular dependencies produce an error with the names of tasks in the cycle

When a dependency fails, all downstream tasks are automatically skipped.

## Context Interpolation

Task fields support `${...}` expressions that resolve at execution time.

Use `${results.task_name.field}` to reference completed task data:

| Expression                            | Returns                                  |
|---------------------------------------|------------------------------------------|
| `${results.task_name.output}`         | The output text of the completed task    |
| `${results.task_name.success}`        | `"true"` or `"false"`                    |
| `${results.task_name.agent}`          | Name of the agent that handled the task  |
| `${results.task_name.duration_ms}`    | Execution time in milliseconds           |

Use `${env.VAR_NAME}` to reference environment variables.

**Automatic dependency injection:** In addition to explicit interpolation, the engine
automatically appends dependency outputs to the prompt (`Result from 'task_name': <output>`).
You do not need `${results...}` for simple chaining -- dependencies are always visible.

## Conditional Tasks

### The `condition` field

A Lua expression evaluated at runtime. The task only executes if it returns a truthy
value. The expression has access to a `results` table where each completed task entry
has `output` (raw string), `success` (boolean), and `agent` (string) fields.

If a task's output is valid JSON, its top-level fields are automatically parsed and
merged into the entry — so you can access nested fields directly:

```lua
-- Task "parse" returned: {"hasUnknowns": true, "speakers": [...]}
-- Both of these work:
condition = "results.parse.success"           -- standard field
condition = "results.parse.hasUnknowns"       -- parsed from JSON output

crew:add_task({
    name = "resolve_unknowns",
    description = "Resolve unknown speakers",
    condition = "results.parse.hasUnknowns",  -- skips if no unknowns
    depends_on = {"parse"},
})
```

### `crew:add_task_if()`

A convenience method that sets the `condition` field for you:

```lua
crew:add_task_if("results.analyze and results.analyze.success", {
    name = "summarize",
    description = "Write a one-sentence summary based on the analysis rating",
    depends_on = {"analyze"},
})
```

This is equivalent to setting `condition` directly in `crew:add_task()`. Skipped
tasks are marked as successful with output `"Skipped: condition '...' evaluated to false"`.

## Error Recovery

The `on_error` field names another task that acts as a fallback. Error handler tasks
are not executed during normal flow -- they only run when the referencing task fails.

```lua
crew:add_task({
    name = "risky_task",
    description = "Attempt something that might fail",
    on_error = "fallback",
    depends_on = {"analyze"},
})

crew:add_task({
    name = "fallback",
    description = "Provide a safe default response",
    agent = "handler",
})
```

When `risky_task` fails:

1. The engine finds the `fallback` task definition
2. Error context is injected: `"Error from task 'risky_task' (agent: ...): <error>"`
3. The fallback task executes
4. If the fallback succeeds, `risky_task` is marked as recovered: `"Recovered via 'fallback': <output>"`
5. If the fallback also fails, `risky_task` remains failed

Error handlers that are never triggered are marked as skipped at the end of the run.

## Foreach Tasks

Foreach tasks iterate over a JSON array, executing the task once per item.
Use `crew:add_foreach_task()`:

```lua
-- Store a list in memory
crew:memory_set("topics", json_stringify({"Rust", "Python", "Go"}))

crew:add_foreach_task({
    name = "analyze_topics",
    description = "Describe the main strength of ${item} as a programming language.",
    foreach = "topics",
    foreach_as = "item",
    agent = "analyst",
})
```

**How it works:**

- `foreach` names a key in results or memory that contains a JSON array
- `foreach_as` (default: `"item"`) sets the variable name used in `${item}` substitution
- Each iteration gets `${item}` replaced with the current array element
- Additional context is injected: `"Processing item 1/3: Rust"`
- Items are processed sequentially
- The final output is a JSON array of individual results
- If any item fails, the overall task still completes but with `success = false`

The source can be a task output (if the output is a JSON array) or a memory key.

## Collaborative Tasks

Collaborative tasks create a multi-turn discussion between two or more agents,
followed by a synthesis step. Use `crew:add_collaborative_task()`:

```lua
crew:add_collaborative_task({
    name = "debate",
    description = "Should teams adopt AI agents for code generation?",
    agents = {"optimist", "critic", "pragmatist"},
    max_turns = 2,
    depends_on = {"research_benefits", "research_risks"},
})
```

**How it works:**

1. Each agent takes a turn responding to the growing conversation, for `max_turns` rounds
2. The first agent in the list performs a final synthesis of the entire discussion
3. Dependency results and memory context are included in the initial prompt
4. Each agent uses its own `system_prompt`, `temperature`, and `model` settings

Requirements:

- The `agents` field must list at least 2 agent names
- All named agents must exist in the crew

## Retry with Exponential Backoff

```lua
crew:add_task({
    name = "answer",
    description = "...",
    max_retries = 2,
    retry_backoff_secs = 1.0,
})
```

The backoff formula is `retry_backoff_secs * 2^attempt` (1s, 2s, 4s, ...).
Both LLM errors and timeouts trigger retries. The total number of attempts is
`max_retries + 1` (the initial attempt plus retries).

## Per-Task Timeout

```lua
crew:add_task({ name = "extract", description = "...", timeout_secs = 60 })
```

If the LLM does not respond within the timeout, the task fails (and may be retried
if `max_retries > 0`). The default timeout is 300 seconds (5 minutes).

## Streaming

When streaming is enabled, LLM response chunks are printed to stderr in real-time.
The complete response is still collected and returned normally. Enable per-task with
`stream = true` or crew-wide via `Crew.new({ stream = true })`.

Streaming is disabled when an agent has tools configured, because tool-calling
requires the full response to process tool invocations.

## Per-Task Model Override

```lua
crew:add_task({ name = "complex", description = "...", model = "gpt-4o" })
```

The model resolution priority is: agent model > task model > model router > crew default.
See [Architecture](architecture.md) for details.
