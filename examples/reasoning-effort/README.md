# Reasoning Effort

Demonstrates the per-agent `reasoning_effort` override: three agents solve the
same logic puzzle at `low`, `medium`, and `high` effort inside one crew, and the
flow prints a side-by-side comparison of duration, token usage, and answer
length. See [docs/agents.md](../../docs/agents.md) for the provider matrix.

## What it does

1. Constructs an `openai-responses` crew whose default effort is `low` and whose
   reasoning summaries are captured (`reasoning_summary = "auto"`).
2. Adds `solver_low`, `solver_medium`, and `solver_high`, each overriding the
   crew default with its own `reasoning_effort`.
3. Runs three independent tasks (they execute in parallel) and prints one row per
   effort level.

## Accepted values

`none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` — validated when the
agent is parsed, so a typo fails at `Crew.new`/`Agent.new` rather than on the
first request. Which values a given model honours is up to the provider:
`gpt-5.6-luna` accepts every value except `minimal` (verified live).

## Provider behaviour

| Provider | Agent `reasoning_effort` |
|---|---|
| `openai-responses` | Sent as `reasoning.effort`; agent value beats the crew value |
| `openai` (Chat Completions) | Forwarded as `reasoning_effort`; a tool-using agent on `gpt-5.6-luna` may only set `"none"` (anything else fails the request with a pointer to `openai-responses`) |
| `anthropic` | Rejected — use the crew-level `thinking_budget` |

## What a run looks like

Observed on `gpt-5.6-luna` via `openai-responses` (2026-09-20; your numbers will
vary — latency in particular is noisy across parallel requests):

| effort | completion tokens | answer |
|---|---:|---|
| `low` | 289 | 36 (correct) |
| `medium` | 342 | 36 (correct) |
| `high` | 359 | 36 (correct) |

Completion tokens include reasoning tokens, so cost rises with effort even
when the visible answer is the same. Only the `high` run emitted a reasoning
summary on this problem. A separate probe confirmed Luna accepts `none`,
`xhigh`, and `max`, and rejects `minimal` with an explicit API error listing its
supported values.

## Run it

```bash
cp examples/reasoning-effort/.env.example examples/reasoning-effort/.env
# Fill in OPENAI_API_KEY in the copied file.

ironcrew validate examples/reasoning-effort
ironcrew run examples/reasoning-effort
# Reasoning summaries per task:
ironcrew run examples/reasoning-effort --json
```
