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

- [ ] **Anthropic Claude provider** — native Claude API support. Claude uses a different message format (`human`/`assistant` roles, system as top-level param). Would need a second provider implementation.

- [x] **Agent hooks** — `before_task` and `after_task` Lua callbacks stored as bytecode. Done in 2.0.1.

- [x] **Parallel foreach** — `foreach_parallel = true`. Done in 1.9.7.

- [x] **Tool timeout** — `IRONCREW_TOOL_TIMEOUT` env var (default 60s). Done in 1.9.7.

- [x] **Pluggable storage backends** — `StateStore` async trait with JSON files (default), SQLite, and PostgreSQL (feature-gated). Done in 2.0.1.

- [ ] **Flow variables / config** — a `config.lua` or `flow.toml` file per project for default settings (model, timeouts, memory limits) so `crew.lua` stays focused on logic.

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

- [ ] **Conversation mode** — multi-turn chat with an agent (not just single-shot tasks). Agent maintains conversation history across turns.

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
