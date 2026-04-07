# Architecture

IronCrew separates the heavy lifting (LLM calls, HTTP, parallel scheduling, tool execution) from your workflow logic (agents, tasks, orchestration). You write Lua - IronCrew compiles to a single native binary that runs it. This page explains how the pieces fit together.

## Three-Layer Design

```
+--------------------------+
|   Lua Scripts            |  crew.lua, agents/*.lua, tools/*.lua
|   (workflow definition)  |
+--------------------------+
|   Lua Bridge             |  Crew.new(), Agent.new(), crew:add_task(), etc.
|   (API surface)          |
+--------------------------+
|   Rust Core              |  Orchestrator, executor, LLM provider, tool registry
|   (engine)               |
+--------------------------+
```

**Rust Core** (`src/engine/`, `src/llm/`, `src/tools/`) -- Handles orchestration, LLM communication, tool execution, dependency resolution, retry logic, and memory. All concurrency and I/O lives here.

**Lua Bridge** (`src/lua/`) -- Exposes Rust functionality to Lua as globals (`Crew`, `Agent`) and methods (`crew:add_task()`, `crew:run()`). Parses Lua tables into Rust structs, registers constructors, and manages the sandboxed Lua environment.

**Lua Scripts** -- User-authored workflow definitions. The entrypoint is `crew.lua`, with optional `agents/` and `tools/` directories for declarative definitions.

## Project Structure

IronCrew supports two modes: **directory mode** and **single-file mode**.

### Directory Mode

```
my-project/
  crew.lua          # Entrypoint (required)
  agents/           # Declarative agent definitions (optional)
    researcher.lua
    writer.lua
  tools/            # Custom tool definitions (optional)
    my_tool.lua
```

When you run `ironcrew my-project/`, the loader:

1. Discovers and loads all `agents/*.lua` files (auto-injected into every `Crew.new()`)
2. Discovers and loads all `tools/*.lua` files (registered in the tool registry)
3. Executes `crew.lua` as the entrypoint

### Single-File Mode

```
ironcrew my-script.lua
```

When given a single `.lua` file, agents and tools are discovered from sibling
directories (`agents/` and `tools/` relative to the script's parent directory).

## Execution Model

### Phases and Topological Sort

Tasks are organized into **execution phases** based on their dependency graph.
The engine uses Kahn's algorithm to:

1. **Validate** the dependency graph (detect missing references, duplicate names, cycles)
2. **Group** tasks into phases -- tasks in the same phase have no dependencies on each other
3. **Execute** each phase, running all tasks within a phase concurrently

```
Phase 0: [task_a, task_b, task_c]    -- no dependencies, run in parallel
Phase 1: [summary]                   -- depends on a, b, c
```

Example from `examples/parallel/crew.lua`:

```lua
crew:add_task({ name = "task_a", description = "..." })
crew:add_task({ name = "task_b", description = "..." })
crew:add_task({ name = "task_c", description = "..." })

crew:add_task({
    name = "summary",
    description = "Compare the three languages...",
    depends_on = {"task_a", "task_b", "task_c"},
})
```

Tasks a, b, and c land in Phase 0 (parallel). The summary task lands in Phase 1.

### Parallel Execution

Within each phase, standard tasks run concurrently using `FuturesUnordered`. This keeps all futures on the current Tokio task (no `tokio::spawn`), so when the orchestrator is cancelled, all in-flight work is dropped immediately.

Concurrency can be bounded with `max_concurrent` in `Crew.new()`:

```lua
local crew = Crew.new({
    goal = "...",
    max_concurrent = 3,  -- at most 3 tasks running simultaneously
})
```

This uses a Tokio semaphore to limit how many tasks execute at once.

### Foreach and Collaborative Tasks

**Foreach tasks** and **collaborative tasks** are handled before the parallel
dispatch within each phase:

- Foreach tasks iterate over a JSON array sequentially, running the LLM once per item
- Collaborative tasks orchestrate a multi-turn discussion between agents, then synthesize

See [Tasks](tasks.md) for full details on these task types.

## Agent Selection

When a task does not specify an `agent` field, the engine auto-selects the best
agent using heuristic scoring. The `AgentSelector` computes a weighted score:

| Component        | Weight | How it works                                                         |
|------------------|--------|----------------------------------------------------------------------|
| Capability match | 0.4    | Fraction of agent's capabilities found as words in the task description |
| Tool match       | 0.3    | Whether agent's tools match tool-related keywords in the description |
| Goal alignment   | 0.3    | Word overlap between agent's goal and the task description           |

Final score: `0.4 * capability + 0.3 * tool + 0.3 * goal`

On a tie, the agent defined earlier wins. If a task explicitly sets `agent = "name"`,
selection is skipped and the named agent is used directly.

See [Agents](agents.md) for agent definition details.

## Context Flow

Context flows between tasks through three mechanisms:

### 1. Interpolation

Task fields (`description`, `expected_output`, `context`) support `${...}` expressions
that resolve at execution time:

- `${results.task_name.output}` -- output text of a completed task
- `${results.task_name.success}` -- `"true"` or `"false"`
- `${results.task_name.agent}` -- agent that handled the task
- `${results.task_name.duration_ms}` -- execution time in milliseconds
- `${env.VAR_NAME}` -- environment variable

Unresolved expressions are replaced with an empty string.

### 2. Dependency Injection

When a task declares `depends_on`, the outputs of those dependencies are automatically
injected into the LLM prompt:

```
Result from 'task_a': <output of task_a>
Result from 'task_b': <output of task_b>
```

This happens in addition to any explicit interpolation in the description or context.

### 3. Memory

The `MemoryStore` provides key-value storage that persists across tasks within a run.
After each phase, successful task results are stored as `task:<name>` keys. Memory
supports two backends:

- **Ephemeral** (default) -- in-memory, lost when the process exits
- **Persistent** -- saved to `.ironcrew/memory.json` on disk

Memory items include metadata (timestamps, access counts, tags, TTL) and are used
for context building via keyword-based relevance scoring. The system evicts items
when limits are exceeded (default: 500 items, 50,000 estimated tokens). Configure
with `memory = "persistent"`, `max_memory_items`, and `max_memory_tokens` in `Crew.new()`.

## Model Resolution

When executing a task, the model is resolved through a priority chain:

1. **Agent's model override** (`agent.model`)
2. **Task's model override** (`task.model`)
3. **Model Router** (purpose-based mapping via `models` table in `Crew.new()`)
4. **Crew's default model** (the `model` field in `Crew.new()`)

```lua
local crew = Crew.new({
    model = "gpt-4.1-mini",        -- default fallback
    models = {
        task_execution = "gpt-4o",
        collaboration = "gpt-4.1-mini",
    },
})
```

## Event System and Run History

The orchestrator emits events throughout execution (`CrewStarted`, `PhaseStart`, `TaskAssigned`, `TaskCompleted`, `TaskFailed`, `TaskSkipped`, `CollaborationTurn`). These power the REST API's Server-Sent Events stream and structured logging.

Each `crew:run()` saves a `RunRecord` with task results, token usage, timing,
tags, and status (success, partial failure, or failed).

Run records are persisted via a pluggable `StateStore` trait:

- **JSON files** (default) — individual `.json` files in `.ironcrew/runs/`
- **SQLite** — single database file at `.ironcrew/ironcrew.db`

Set `IRONCREW_STORE=sqlite` to switch backends. See [CLI Reference](cli.md) for
all storage env vars and [Best Practices](best-practices.md) for guidance on
choosing a backend.
