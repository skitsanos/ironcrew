# Usage accounting (IC-046, in progress)

This page describes the Rust `ironcrew::usage` contract, built-in HTTP capture,
and process-local execution ownership. OpenAI Chat, Responses and Anthropic
retain checked receipts independently of their output return value.

**`ChatResponse.usage`, task/run result fields, CLI/HTTP output fields, events and
stored run/session records have not yet migrated.** Those still have the gaps
tracked by [IC-046](issues/IC-046.md). New Lua `:usage()` accessors expose checked
process-local snapshots separately from those existing fields.
The old usage fields are not a fallback source for the new tracker. Do not use
transport tests as proof of end-to-end billing observability or an IC-047 budget.

## Request-scoped HTTP capture

Rust callers set `request.usage_tracker = Some(tracker.clone())` before calling
any built-in provider's `chat`, `chat_with_tools` or `chat_stream`, and read
`tracker.snapshot()` afterward, including after an error. Sharing that tracker
across retries or concurrent calls includes each dispatched attempt once.
`Agent::chat_request` leaves the scope unset; callers must choose its ownership
explicitly. The scope is never serialized into the provider request or retained
globally. Direct custom-provider calls follow their implementation's contract;
the execution wrapper described below accounts for non-reporting providers.

## Execution scopes

The runtime automatically creates an enclosing scope for each executing Lua VM,
with disjoint child scopes for each crew run and each conversation/dialog handle.
Agent delegation, `run_flow`, `crew:subworkflow`, and Lua-tool child VMs inherit
their caller's scope explicitly. Descendants update inclusive ancestors directly.
Task retries, parallel/foreach tasks and collaborative discussion/synthesis
therefore settle individual provider attempts into the same flow total, without
adding child task results again. Failed output validation and transcript rollback
do not roll back provider usage. Separate top-level VMs remain isolated even
when they share one `Runtime` and its provider. No task-local or global counter
is used. A flow with multiple `crew:run()` calls shares a flow total while each
run has its own subtotal. Independent per-task subtotals remain pending.

For Rust embedding, bind a tracker with
`ironcrew::llm::scope::with_usage_tracker(provider, tracker.clone())` and pass
the returned provider into crew/conversation/dialog execution. Retain the
tracker to inspect it after success, error or cancellation. Unbound crew runs,
conversation/dialog handles and agent turns create an isolated scope themselves.
Lua embedders can inspect the VM's `UsageTracker` app data after execution or
preinstall a tracker to retain access after dropping the VM. A reused VM or
conversation handle accumulates its process-local lifetime, not historical
usage recovered from persistent storage.

An explicit `ChatRequest.usage_tracker` takes precedence over a provider's
bound scope. `ToolCallContext.usage_tracker` likewise chooses the scope for
delegated agent/conversation calls, including no-tools streaming. Wrapping a
provider twice does not double-count requests or merge unrelated scopes.

Custom `LlmProvider` implementations opt into checked receipt ownership by
returning `true` from `records_usage()` and settling the supplied request tracker
once per actual dispatch, including errors and cancellation. Providers without
that contract have each invocation counted as **unavailable** by the execution
wrapper after its offline validation hook succeeds. Their old `TokenUsage`
fields are deliberately not converted into checked receipts. This opaque
invocation boundary cannot reveal internal HTTP retries or billing details.
Forwarding wrappers must preserve both `records_usage()` and `usage_tracker()`.

These snapshots are not durable recovery, migrated task/API result fields,
per-task attribution, token-budget enforcement, or proof of replica behavior.
Those acceptance boundaries remain open.

Built-in HTTP attempts start after local validation and rate-limit waiting, immediately
before HTTP dispatch. Invalid credentials/options/URLs rejected before dispatch
do not invent a request. A dispatched attempt without a receipt settles as
unavailable; timeout or cancellation does not prove zero provider cost.

Non-streaming receipt capture precedes output decoding. Streaming capture
precedes content assembly and output-channel awaits. Final unsuccessful
Responses events and bounded HTTP error bodies can still carry valid receipts.
Cancellation drops the attempt guard and retains its last observed snapshot.
Aggregate overflow fails normal completion and remains a sticky tracker error
after cancellation; it is never silently saturated.

Chat streaming requests usage and recognizes its usage-only terminal chunk.
Responses requires a terminal response status/event with a usage object;
missing/queued/in-progress statuses cannot prove final accounting. Anthropic
requires a message-delta output receipt followed by message-stop. Later
nonterminal usage updates cannot inherit an earlier terminal coverage claim.
Truncation and missing terminal receipts preserve partial or unavailable usage.

## Receipt contract

`UsageReceipt` describes one provider attempt, independently of task success.
`UsageCounts` uses unsigned 64-bit integers in Rust and explicit `None`/JSON `null` for
unknown fields. Zero is known only when actually reported or derived entirely
from known zero categories. Missing reasoning/cache detail is never guessed.

- `complete`: a final receipt has known, consistent input/output/total counts.
- `partial`: some usage is known, but final accounting is incomplete or invalid.
- `unavailable`: no supported counts are known.

Optional detail coverage is separate from primary coverage. Reasoning tokens
are part of completion tokens, not an extra amount to add to the total. Cache
reads and writes are input categories. A reasoning-token count is not reasoning
summary text, and visible text length is not a substitute for a provider receipt.

Parsers retain known fields without coercing strings, fractions, negative
numbers, booleans or malformed nested objects into counts. Contradictory totals
and out-of-range subsets become unknown. Arithmetic never narrows to 32 bits.
Serialized receipts round-trip with explicit unknowns, and deserialization
rejects contradictory counts or forged coverage.

The checked receipt/aggregate/snapshot wire format uses canonical unsigned
decimal **strings**, including request and in-flight counts. This preserves the
entire `u64` range in JSON, JavaScript and Lua without narrowing. Unknown counts
are `null`, not `"0"`. Numeric JSON values, leading zeros, signs, fractions,
overflow, unknown fields and inconsistent coverage are rejected on read. Raw
provider parsers still consume the provider's numeric protocol; there is no
compatibility adapter from the old public usage fields. HTTP and SQL migration
remains pending; the old PostgreSQL `INTEGER` columns cannot store this range.

## Lua snapshots

| Method | Scope |
|---|---|
| `crew:usage()` | Latest run that entered execution; `nil` before any such run |
| `crew:flow_usage()` | Inclusive enclosing VM/caller scope, including all its crews and sessions |
| `conversation:usage()` | Calls owned by this conversation handle since construction/resume |
| `dialog:usage()` | Calls owned by this dialog handle since construction/resume |

Each call returns a detached snapshot, not a live mutable view. A new run replaces
the crew's last-run view; earlier returned snapshots stay unchanged. Nested calls
are already included in ancestors: never add `crew:usage()` to `crew:flow_usage()`.
Explicit Rust request/tool-context scope overrides choose a different owner and
are intentionally excluded from a handle's default scope. Resuming a persisted
session creates an empty process-local scope; it does not recover historical usage.

```lua
local ok, result = pcall(function() return crew:run() end)
local usage = crew:usage()
if usage then
    print(json_stringify(usage)) -- also inspect after failure/cancellation
    local total = usage.settled.total_tokens
    if type(total.known) == "string" then
        print(total.known, total.complete, usage.coverage)
    end
end
```

Shape: `{ settled = { requests, coverage, prompt_tokens, completion_tokens,
total_tokens, cached_tokens, cache_write_tokens, reasoning_tokens }, in_flight,
coverage }`. Every token field is `{ known, complete }`. In Lua, unknown `known`
values use the serializer's null sentinel so `json_stringify` emits explicit
`null` instead of dropping the field. Test `type(count.known) == "string"` for
a known subtotal. Do not convert large counts with `tonumber`; arithmetic and
budget enforcement belong in checked Rust code. Snapshot reads fail visibly
after accounting overflow. These accessors neither persist data nor enforce budgets.

## Provider mapping

The fields were checked against official documentation on 2026-09-20:

| Normalized count | OpenAI Chat Completions | OpenAI Responses | Anthropic Messages |
|---|---|---|---|
| Input | `prompt_tokens` | `input_tokens` | `input_tokens` + `cache_read_input_tokens` + `cache_creation_input_tokens` |
| Output | `completion_tokens` | `output_tokens` | `output_tokens` |
| Total | `total_tokens` | `total_tokens` | Normalized input + output |
| Cache read | `prompt_tokens_details.cached_tokens` | `input_tokens_details.cached_tokens` | `cache_read_input_tokens` |
| Cache write | `prompt_tokens_details.cache_write_tokens` | `input_tokens_details.cache_write_tokens` | `cache_creation_input_tokens` |
| Reasoning | `completion_tokens_details.reasoning_tokens` | `output_tokens_details.reasoning_tokens` | `output_tokens_details.thinking_tokens` |

OpenAI's output count includes reasoning and non-visible formatting tokens.
See [output token counts](https://developers.openai.com/api/docs/guides/token-counting#understand-output-token-counts)
and [reasoning usage](https://developers.openai.com/api/docs/guides/reasoning#controlling-costs).
Cache write/read counts are reported separately in the current
[Chat Completions schema](https://developers.openai.com/api/reference/resources/chat/subresources/completions/streaming-events#chat.completion.chunk)
and [prompt-caching guide](https://developers.openai.com/api/docs/guides/prompt-caching).

Anthropic's base `input_tokens` excludes cache categories; summing all three
is necessary. Missing categories produce only a partial known subtotal, not a
complete total. See [cache accounting](https://platform.claude.com/docs/en/build-with-claude/prompt-caching#how-do-i-calculate-total-input-tokens-from-the-usage-fields)
and the [Messages usage schema](https://platform.claude.com/docs/en/api/messages/create)
for the optional thinking detail.

`StreamUsage` retains only six scalar fields, not payloads or an event list.
It replaces cumulative snapshots rather than summing them. Anthropic reports
cumulative usage in [message deltas](https://platform.claude.com/docs/en/build-with-claude/streaming#event-types).
Chat Completions needs `stream_options.include_usage`; interruption may prevent
the final receipt from arriving, as the [stream schema](https://developers.openai.com/api/reference/resources/chat/subresources/completions/streaming-events#chat.completion.chunk)
documents. Null usage chunks do not erase earlier usage. Malformed/regressing
updates preserve previous known subtotals but prevent complete coverage.
Transport adapters must identify terminal accounting correctly, including
terminal unsuccessful responses; content success alone is not that signal.

## Aggregation and ownership

`UsageAggregate` adds one receipt per attempt, including failed attempts and
retries. Every field has `{ known, complete }`, so a partial subtotal is not
mistaken for a fully measured amount. An empty aggregate has zero requests and
is distinct from one request with an unavailable receipt. Checked addition is
atomic: overflow returns `UsageOverflow` without partially modifying totals.
It must never be ignored or converted into a successful zero count.

`UsageTracker` is a process-local shared accounting scope with constant-size
counters per node and a maximum of 64 child levels. Its
attempt guard settles exactly once on completion or drop, including cancellation
of a polled async request. Clones share a scope; separately created trackers do
not. `child()` creates a disjoint child view and updates its inclusive ancestors
atomically, with root-first locks. A nesting-limit failure rejects the new scope
before dispatch. Retries and delegated calls use separate guards on the run scope
or a descendant.
Do not add child results again to a tracker that already includes child requests.
Explicit aggregate merges are only for disjoint scopes.

In-flight receipts are retained in the attempt guard until settlement, so a
snapshot reports outstanding requests and cannot claim complete coverage while
any remain. Stream code must observe the latest cumulative snapshot before an
await that could cancel it. Drop-time arithmetic failure leaves a sticky error
visible to snapshots and new attempts. Process death, durable checkpointing,
distributed ownership, lease fencing and journal publication are not implemented
by this in-memory helper. It also does not reserve or enforce a token budget.

These are token measurements, not invoices. They exclude provider-side activity
for which no receipt reaches IronCrew, non-token tool charges, billing tiers,
credits and monetary reconciliation. Partial or unavailable usage must stay
visible through the upcoming CLI/Lua/HTTP/store integration.

## Remaining integration

1. Replace the old response/task usage types and add per-task views without
   double-counting inclusive run/flow totals. Do not add a compatibility
   fallback to old receipt fields.
2. Carry the same coverage contract through result fields, CLI, HTTP/events, run/session
   persistence and JSON/SQLite/PostgreSQL. Preserve owner fencing and terminal
   compare-and-set behavior and the lossless decimal-string wire representation.
3. Run durable-backend acceptance against disposable PostgreSQL 15 and the
   complete affected gates before resolving IC-046 or implementing IC-047.
