# agent-as-tool — per-turn specialist delegation

Demonstrates the v2.14.0 `agent__<name>` primitive. The `coordinator`
agent lists two specialists in its `tools` array; its LLM invokes
them the same way it invokes any other tool. Each invocation runs the
specialist's own tool-call loop in a fresh, ephemeral context.

## Layout

```
examples/agent-as-tool/
├── crew.lua   # coordinator + researcher + writer, all in one crew
└── README.md
```

## What happens

1. The single `answer` task is assigned to the `coordinator` agent.
2. The coordinator's LLM sees `agent__researcher` and `agent__writer`
   as tool schemas alongside any built-in tools.
3. Following its system prompt, it calls `agent__researcher` with a
   focused prompt.
4. The researcher runs its own LLM turn (its own system prompt, its
   own tools) and returns bullets.
5. The coordinator then calls `agent__writer` with those bullets.
6. The writer returns a paragraph — the coordinator relays it.

Each sub-agent invocation is **ephemeral**: fresh message history per
call, no persistence, no autosave. On the event stream each
invocation is bracketed by `AgentToolStarted` and `AgentToolCompleted`
events so chat UIs can scope the nested `tool_call` / `tool_result`
activity under a single marker.

## Run it

```sh
export GEMINI_API_KEY=...    # provider credential
ironcrew run examples/agent-as-tool
```

Pass a custom question:

```sh
ironcrew run examples/agent-as-tool \
  --input '{"question":"Why does Go prefer goroutines over threads?"}'
```

## Under the hood

- Nested depth is capped by `IRONCREW_MAX_FLOW_DEPTH` (default `5`),
  shared with `run_flow` — an agent-as-tool call and a `run_flow` call
  count against the same budget.
- Agent name format: `^[a-z][a-zA-Z0-9_-]*$`, composed `agent__<name>`
  capped at 64 characters.
- See `docs/agents.md#agent-as-tool` for the full reference.

## When to use which primitive

| Need                                           | Use                              |
| ---------------------------------------------- | -------------------------------- |
| "Ask one specialist a question"                | `agent__<name>`                  |
| "Run a multi-step pipeline with dependencies"  | `run_flow` / `crew:subworkflow`  |

See `docs/tools.md#delegation-primitives` for the full comparison.
Compare `examples/chat-http/` for the `run_flow` / sub-crew version
of a coordinator-driven multi-agent pattern.

## Environment

| Variable                  | Default                                                        | Purpose                              |
| ------------------------- | -------------------------------------------------------------- | ------------------------------------ |
| `GEMINI_API_KEY`          | —                                                              | Provider credential                  |
| `GEMINI_MODEL`            | `gemini-2.5-flash`                                             | Override the default model           |
| `GEMINI_BASE_URL`         | `https://generativelanguage.googleapis.com/v1beta/openai`      | Override the provider endpoint       |
| `IRONCREW_MAX_FLOW_DEPTH` | `5`                                                            | Shared depth cap for nested calls    |
