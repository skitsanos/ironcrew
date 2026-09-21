# Run token budgets (IC-047)

`IRONCREW_MAX_RUN_TOKENS` is an opt-in operator ceiling for provider generation
requests owned by one execution. It counts **input plus output tokens**, including
cached input and reasoning output once, not dollars. Without the variable,
`usage.budget.state` is explicitly `disabled`.

```bash
IRONCREW_MAX_RUN_TOKENS=100000 ironcrew run examples/providers/02-openai-responses.lua
IRONCREW_MAX_RUN_TOKENS=100000 ironcrew serve --flows-dir examples
```

The value must contain only decimal digits and be between **1 and
1,000,000,000**, inclusive. Empty, zero, negative, non-integer, non-UTF-8 and
out-of-range values fail closed. There is no `0`/`none` disable spelling: unset
the variable to disable the policy. CLI run/server startup validates it before
work admission. Embedded runtime scopes validate it when created.
Lua configuration and request bodies cannot raise the operator ceiling.

## Supported admission contract

The first supported transport is **native OpenAI Responses** at
`https://api.openai.com`, without provider-hosted tools. Ordinary IronCrew
function tools, delegated agents and subflows remain supported: each additional
generation request is counted and admitted separately. Chat Completions,
Anthropic, compatible proxy origins and provider-hosted web/file search, code
interpreter or MCP tools are rejected when budgeting is enabled. They keep their
ordinary behavior with budgeting disabled. Rejection occurs before generation.

For each supported request, IronCrew:

1. Builds and validates one generation payload, with full history and inline
   images. Mutable server-side history references are unsupported.
2. Sends its input-bearing fields to OpenAI's
   [input-token counting endpoint](https://developers.openai.com/api/docs/guides/token-counting).
   Instructions, function schemas, structured output and reasoning settings
   accompany the same input. There is no character-length estimate fallback.
3. Atomically reserves the returned input count plus `max_output_tokens` from
   the shared budget immediately before dispatch. An explicit agent `max_tokens`
   is preserved; otherwise budgeted requests use **4,096 output tokens**. A
   reservation that does not fit is rejected, not silently shortened.
4. On a complete, within-bound receipt, charges actual input plus output and
   releases only the unused portion. Missing/partial receipts or cancellation
   retain the entire reservation. Error responses with complete receipts can
   reconcile even when generation/output processing failed.

The counting request has the same credential/origin and outbound-network policy,
bounded request bytes, a response cap of 4 KiB, and a timeout no longer than 15
seconds or the configured provider timeout. It obeys rate limiting and has no
automatic retry. Counting failure is a sticky run-budget failure; no generation
follows it. Counting calls are preflight HTTP operations, not generation receipts
in `usage.settled.requests`.

The output bound includes reasoning tokens, as documented for Responses
[`max_output_tokens`](https://developers.openai.com/api/reference/resources/responses/methods/create).
The guarantee depends on the provider honoring its count and output bound.
If any reported input/output/total exceeds its reservation, IronCrew records
the actual receipt, retains the reservation and blocks further dispatches with
`bound_violated`. It cannot undo an already-billed provider overrun.

## Ownership and failure

- One CLI `run` or HTTP flow entrypoint owns one budget. Multiple `crew:run()`
  calls in that Lua VM share it, as do retries, parallel/foreach/collaborative
  tasks, dialogs, conversations, delegated agents and child Lua flows.
- CLI interactive chat shares its execution's budget for the lifetime of that
  chat process. Clearing transcript history does not refill the budget.
- Each standalone HTTP conversation message receives a fresh per-request
  budget. It covers that message's tool rounds and delegated work, **not** the
  session's lifetime. Rehydrating history does not charge historical tokens to
  the new request; sending that history to the model does consume input tokens.
- Concurrent requests reserve under one lock. A denied reservation makes the
  failure sticky even if another in-flight request subsequently frees capacity.
  Already-dispatched requests may finish within their existing reservations.
- A successful request that spends the exact remaining capacity may finish;
  the next request cannot dispatch. Budget failures are non-retriable and do
  not become successful task/run output if Lua catches their errors.

Budgets are process-local admission identities, not distributed account quotas.
Different runs/replicas have independent ceilings; aggregate possible consumption
scales with admitted runs. There is no execution failover, replenishment across
owner death or guarantee of exact currency cost. Ordinary HTTP/tools invoking
external model services outside IronCrew's provider interface are outside this
ceiling. Provider pricing, hosted services and preflight HTTP charges are not
represented as a money limit.

## Inspection and persistence

Checked usage snapshots include a required `budget` object. Counts remain
decimal strings, preserving the existing lossless JSON/Lua contract:

Pre-budget snapshots without this required field are unsupported, not silently
treated as unbudgeted records. This is a strict wire-contract change; no legacy
counter conversion or automatic rewriting of existing user history is performed.

```json
{
  "state": "active",
  "limit": "100000",
  "charged": "3200",
  "retained": "1000",
  "reserved": "4096",
  "in_flight": "1"
}
```

`charged` is settled capacity, including conservative reservations retained for
incomplete attempts; `retained` is that conservative subset. `reserved` belongs
to currently dispatched attempts. Available capacity is
`limit - charged - reserved`. These are capacity counters, not billing totals;
use the separate checked receipt fields for observed usage.

States are `disabled`, `active`, `exhausted`, `unsupported`, `counting_failed`,
`bound_violated`, and `unavailable` when no trustworthy checkpoint exists.
Disabled/unavailable snapshots have a null limit and zero capacity counters.

CLI run history/inspection, Lua `crew:usage()` / `crew:flow_usage()`, task/run
events, HTTP run responses and JSON/SQLite/PostgreSQL terminal records retain
the snapshot. Child task snapshots show their shared run budget at capture
time, not a second independent allowance. No in-flight durable budget journal
or restartable reservation is claimed.

HTTP conversation message responses add `request_usage`, including their
per-message budget. Their existing `usage` remains session-lifetime receipts;
session history does not own a lifetime budget and shows `disabled`. Successful
idempotent message responses persist and replay the original `request_usage`.
Budget errors return HTTP 422 with a safe error and budget snapshot. Failed
messages do not create a success-shaped transcript checkpoint: unsaved session
receipts retain the existing live-handle/checkpoint durability boundary.

Rust embedders can create `UsageTracker::for_run()` from operator configuration
and bind it with `llm::scope::with_usage_tracker`. Direct provider requests need
an explicit shared tracker when the operator ceiling is enabled. Trusted custom
providers must implement the `supports_token_budget` admission contract as well
as checked receipt ownership; forwarding wrappers must preserve the capability.
`UsageTracker::from_snapshot` restores historical receipts, not budget ownership;
always create/bind a new enclosing run scope for resumed work. Trusted Rust
hosts can construct explicit policies; Lua flow authors cannot replace them.
