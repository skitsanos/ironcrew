# IronCrew — Roadmap

Sorted by value/effort ratio: high-value low-effort items first.

---

## High Value, Low Effort

- [x] **`ironcrew run` with `--input` flag** — pass JSON input from CLI. Done in 1.9.6.
- [x] **`print()` routing** — stdout in CLI, SSE-only in API mode. Done in 1.9.6.
- [x] **Rate limiting** — `IRONCREW_RATE_LIMIT_MS` env var. Done in 1.9.6.
- [x] **Condition evaluator JSON parsing** — access nested fields from task output. Done in 1.9.4.
- [x] **SSE run status fix** — use saved run record status, not Lua exit status. Done in 1.9.7.
- [x] **Configurable max run lifetime** — `IRONCREW_MAX_RUN_LIFETIME` env var (seconds). Done in 1.9.7.
- [x] **JSON output mode** — `ironcrew run . --json` outputs structured JSON. Done in 1.9.7.
- [x] **Tracing to stderr** — logs no longer mix with stdout output. Done in 1.9.7.

- [x] **Task output truncation in SSE** — `IRONCREW_SSE_OUTPUT_MAX_CHARS` env var (disabled by default). Done in 1.9.7.

- [x] **Bearer authentication for REST API** — `IRONCREW_API_TOKEN` env var. `/health` stays public. Done in 1.9.8.

- [x] **`ironcrew doctor`** — diagnostic command checking env vars, project structure, Lua syntax. Done in 1.9.7.

---

## High Value, Medium Effort

- [x] **Anthropic Claude provider** — native Messages API with server-side tools (web_search, code_execution), extended thinking, and block-based streaming. Done in 2.3.0.

- [x] **OpenAI Responses API provider** — native `/v1/responses` endpoint with reasoning items, built-in server-side tools (web_search, file_search, code_interpreter), and block-based streaming. Works with OpenAI, Azure, xAI/Grok, OpenRouter. Done in 2.3.0.

- [x] **Cross-provider reasoning/thinking capture** — unified support for Anthropic thinking blocks, OpenAI Responses reasoning items, DeepSeek `reasoning_content`, Kimi/Moonshot `reasoning_content`. Streams dim to stderr, persists to run records, emits `task_thinking` SSE events. Done in 2.3.0.

- [x] **Extended provider support** — URL-based auto-detection for Kimi/Moonshot, DeepSeek, xAI/Grok, and OpenRouter. Done in 2.3.0.

- [x] **Agent hooks** — `before_task` and `after_task` Lua callbacks stored as bytecode. Done in 2.0.1.

- [x] **Parallel foreach** — `foreach_parallel = true`. Done in 1.9.7.

- [x] **Tool timeout** — `IRONCREW_TOOL_TIMEOUT` env var (default 60s). Done in 1.9.7.

- [x] **Pluggable storage backends** — `StateStore` async trait with JSON files (default), SQLite, and PostgreSQL (feature-gated). Done in 2.0.1.

- [x] **Flow variables / config** — `config.lua` at the project root provides default settings (provider, model, limits, router, reasoning, server tools) shallow-merged into `Crew.new()` so `crew.lua` stays focused on workflow. Done in 2.4.0.

- [ ] **Image input support** — pass images to vision-capable models (GPT-4o, Gemini). Would need a `content` array in ChatMessage instead of a plain string.

---

## Production Readiness (Done)

- [x] **CORS configuration** — `IRONCREW_CORS_ORIGINS` (deny-all default). Done in 2.1.0.
- [x] **Graceful shutdown** — SIGTERM/Ctrl+C for Kubernetes. Done in 2.1.0.
- [x] **SSRF protection** — blocks private IPs in HTTP tool + Lua http.*. Done in 2.1.0.
- [x] **Request/response size limits** — `IRONCREW_MAX_BODY_SIZE`, `IRONCREW_MAX_RESPONSE_SIZE`. Done in 2.1.0.
- [x] **Env var security** — `env()` blocks sensitive vars (`*_API_KEY`, `*_SECRET`, etc.). Done in 2.1.0.
- [x] **Prompt size limit** — `IRONCREW_MAX_PROMPT_CHARS` (default 100KB). Done in 2.1.0.
- [x] **Default concurrency cap** — always applies semaphore (default 10). Done in 2.1.0.
- [x] **EventBus/MessageBus optimization** — Arc-wrapped events, VecDeque, configurable cap. Done in 2.1.0.
- [x] **Lua VM pooling** — thread-local reuse for hooks and conditions. Done in 2.1.0.
- [x] **Shared HTTP client** — singleton reqwest::Client. Done in 2.1.0.
- [x] **Regex caching** — thread-local cache for Lua regex globals. Done in 2.1.0.
- [x] **API error sanitization** — no filesystem paths in responses. Done in 2.1.0.
- [x] **Directory permissions** — `.ironcrew/` set to 0o700 on Unix. Done in 2.1.0.
- [x] **PG hardening** — table prefix validation, configurable pool size. Done in 2.1.0.

---

## Medium Value, Low Effort

- [x] **Run tags/labels** — `--tag` flag on run, tags in API input, stored in run record. Done in 2.0.0.

- [x] **`ironcrew fmt`** — static Lua lint: syntax, agent/tool validation, unknown tool warnings. Done in 2.0.0.

- [x] **`ironcrew export`** — package flow as standalone directory with .env.template. Done in 2.0.0.

---

## Medium Value, Medium Effort

- [x] **Conversation mode** — single-agent multi-turn chat via `crew:conversation({...})` with tool support, streaming to stderr, reasoning capture, and `max_history` cap. Done in 2.4.0.
- [x] **Agent-to-agent conversations** — `crew:dialog({})` runs perspective-flipped two-agent dialogs (each agent sees its own turns as assistant, opponent's as user with `[name]:` prefix). Includes `run`, `next_turn`, `reset`, transcript inspection. Done in 2.4.0.
- [x] **Conversation/Dialog SSE wiring** — both primitives emit dedicated events (`conversation_started`/`turn`/`thinking` and `dialog_started`/`turn`/`thinking`/`completed`) through the EventBus. REST API subscribers see them in real time alongside task events. Each event includes a stable `conversation_id` / `dialog_id`. Done in 2.4.0.
- [x] **Multi-party dialogs** — `crew:dialog({agents = {...}})` supports 2+ agents in round-robin order. Speaker tracked by index, SSE events use positional letter labels (`"a"`, `"b"`, `"c"`, ...). Backward compatible with the legacy `agent_a`/`agent_b` form. Done in 2.4.0.
  - [ ] **Moderator-driven dialogs** — let a separate agent (or Lua callback) decide who speaks next instead of round-robin
  - [ ] **Custom termination** — Lua callback to end a dialog early (e.g., on agreement detection)
  - [ ] **Cross-run persistence** — save/load conversation state by ID

- [ ] **Cost estimation** — pre-run estimate of token usage and cost based on prompt sizes and model pricing.

- [ ] **Run comparison** — diff two run results to see what changed. Useful for A/B testing prompts or models.

- [ ] **Encrypted persistent memory** — encrypt memory.json at rest for sensitive data.

- [ ] **Structured run summary** — `GET /flows/{flow}/runs/{id}` with task counts, total tokens, total duration — not just raw results.

---

## Lower Priority / Exploratory

- [ ] **MCP (Model Context Protocol)** — support for MCP tool servers.
- [ ] **WebSocket transport** — bidirectional communication with running crews.
- [ ] **DAG visualization** — `ironcrew graph .` generates Mermaid/DOT diagram.
- [ ] **Hot reload** — watch Lua files in serve mode, reload without restart.
- [ ] **Plugin system** — load custom Rust tools from shared libraries.
- [ ] **Crates.io publish** — `cargo install ironcrew`.
- [ ] **WASM target** — browser-based agent orchestration.
