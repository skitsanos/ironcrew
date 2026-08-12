# IronCrew Examples

Each runnable flow is a directory containing `crew.lua`, except the standalone
provider reference files under `providers/`. From the repository root:

```bash
cp examples/simple/.env.example examples/simple/.env
# Replace the provider key in the copied file.
ironcrew validate examples/simple
ironcrew run examples/simple
```

Provider keys are read directly by IronCrew. Values accessed from Lua with
`env()` must also appear in `IRONCREW_ENV_ALLOWLIST`; the checked-in
`.env.example` files show the required names.

## Start here

| Example | Demonstrates |
|---|---|
| [`simple`](simple/) | Minimal agent, task, and run |
| [`research-crew`](research-crew/) | File-defined agents and a custom Lua tool |
| [`config-lua`](config-lua/) | Project-wide defaults in `config.lua` |
| [`shared-modules`](shared-modules/) | Sandboxed `require()` and an offline sub-flow; no LLM required |
| [`json-output`](json-output/) | JSON Schema output, UUIDs, JSON helpers, and file writing |

## Human control and conversations

| Example | Demonstrates |
|---|---|
| [`ask-human`](ask-human/) | Flow-authored CLI/HTTP questions, SSE, and answers |
| [`human-approval`](human-approval/) | Agent-facing `ask_human` plus real `require_approval` tool gates |
| [`chat-cli`](chat-cli/) | Interactive single-agent chat and stable session ids |
| [`chat-http`](chat-http/) | HTTP chat plus a callable sub-crew |
| [`chat-ui`](chat-ui/) | Bun/React UI companion for `chat-http` |
| [`conversation`](conversation/) | Stateful multi-turn conversation from Lua |
| [`cross-run-persistence`](cross-run-persistence/) | Flow-scoped conversation and dialog resume |
| [`dialog-early-stop`](dialog-early-stop/) | Dialog termination callback |
| [`moderator-dialog`](moderator-dialog/) | LLM turn selector or explicit `next_turn_from` loop |
| [`roundtable`](roundtable/) | Multi-participant dialog |
| [`stock-debate`](stock-debate/) | Larger debate workflow with structured synthesis |

## Orchestration and data flow

| Example | Demonstrates |
|---|---|
| [`parallel`](parallel/) | Parallel topological phases |
| [`foreach`](foreach/) | Foreach task expansion |
| [`conditional-crew`](conditional-crew/) | Conditions and error routing |
| [`batch-processing`](batch-processing/) | Batch files, schema validation, and templates |
| [`collaborative`](collaborative/) | Collaborative task plus MessageBus send/read/history |
| [`memory`](memory/) | Shared crew memory |
| [`subworkflow`](subworkflow/) | Nested sub-workflow execution |
| [`agent-as-tool`](agent-as-tool/) | Specialist agents exposed as tools |
| [`model-router`](model-router/) | Purpose-based model routing |
| [`streaming`](streaming/) | Streamed model output |
| [`vision`](vision/) | Image input to a vision-capable model |
| [`http-api`](http-api/) | Lua `http.get`/`http.post` and templates |
| [`groq-json`](groq-json/) | Structured JSON through Groq |

## Providers

- [`providers`](providers/) contains 12 standalone reference files for OpenAI
  Chat, OpenAI Responses, Anthropic, Gemini, Groq, Kimi, and DeepSeek.
- [`anthropic`](anthropic/) covers native Claude, web search, and thinking.
- [`responses`](responses/) covers OpenAI Responses, web search, and reasoning.
- [`gemini`](gemini/) is a fuller Gemini workflow.

## MCP and supporting material

- [`mcp/git-tools`](mcp/git-tools/) uses a local stdio MCP server.
- [`mcp/http-tools`](mcp/http-tools/) uses Streamable HTTP MCP.
- [`mcp/plufinder`](mcp/plufinder/) is an externally dependent MCP example.
- [`graph-prototype`](graph-prototype/) is the static prototype behind graph
  visualization assets.
- [`subflow_stub`](subflow_stub/) is a hermetic integration-test fixture, not a
  provider-backed showcase.

Examples that use providers or public APIs require network access. CI validates
every checked-in Lua file and executes only deterministic offline probes.
