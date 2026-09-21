# Usage accounting (IC-046)

Operator-controlled admission is documented separately in
[run token budgets](token-budgets.md). Every snapshot includes its budget state;
receipt coverage and budget capacity are distinct measurements.

This page describes the Rust `ironcrew::usage` contract, built-in HTTP capture,
and process-local execution ownership. OpenAI Chat, Responses and Anthropic
retain checked receipts independently of their output return value.

Task results, run records/summaries, CLI/HTTP outputs and task/run events now
expose checked `usage` snapshots. JSON, SQLite and PostgreSQL persist these
snapshots without narrowing counts. `ChatResponse.usage` is a checked
`UsageReceipt`; conversation/dialog checkpoints retain checked session history.
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
run has its own subtotal. Each task has a disjoint subtotal including its retries,
tool calls and delegated work. Foreach includes every item; collaboration includes
discussion and synthesis. Error-handler attempts belong to the handler result,
not the recovered task's subtotal. Failed handlers retain their receipts too.

For Rust embedding, bind a tracker with
`ironcrew::llm::scope::with_usage_tracker(provider, tracker.clone())` and pass
the returned provider into crew/conversation/dialog execution. Retain the
tracker to inspect it after success, error or cancellation. Unbound crew runs,
conversation/dialog handles and agent turns create an isolated scope themselves.
Lua embedders can inspect the VM's `UsageTracker` app data after execution or
preinstall a tracker to retain access after dropping the VM. A reused VM
accumulates its process-local lifetime. Conversation/dialog usage includes
its restored durable checkpoint plus newly observed calls.

An explicit `ChatRequest.usage_tracker` takes precedence over a provider's
bound scope. `ToolCallContext.usage_tracker` likewise chooses the scope for
delegated agent/conversation calls, including no-tools streaming. Wrapping a
provider twice does not double-count requests or merge unrelated scopes.

Custom `LlmProvider` implementations opt into checked receipt ownership by
returning `true` from `records_usage()` and settling the supplied request tracker
once per actual dispatch, including errors and cancellation. Providers without
that contract have each invocation counted as **unavailable** by the execution
wrapper after its offline validation hook succeeds. A returned receipt alone
does not opt a custom provider into full attempt accounting. This opaque
invocation boundary cannot reveal internal HTTP retries or billing details.
Forwarding wrappers must preserve `records_usage()`, `usage_tracker()` and
`records_usage_metrics()` to avoid duplicate accounting or telemetry.

Terminal run snapshots are durable; in-flight attempts are not checkpointed.
These snapshots are not token-budget enforcement or execution failover.

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
compatibility adapter from the old public usage fields. PostgreSQL stores the
snapshot as JSONB and SQLite as JSON text, not signed token-count columns.
SQL readers reject usage payloads above 4 KiB before client-side materialization;
the fixed checked snapshot shape fits comfortably below this bound.

## Result and persistence boundaries

`TaskResult.usage` replaces `token_usage`; run records/summaries and `run_complete`
replace the old top-level `total_tokens`/`cached_tokens` counters with `usage`.
`task_completed` and `task_failed` include the same checked task snapshot.
Known subtotals are retained even when coverage is partial or unavailable for
other fields. Do not add child snapshots to an already inclusive parent.

CLI run records finish at `crew:run()` and contain that crew's subtotal. HTTP
runs own the complete Lua entrypoint: their terminal snapshot includes calls
before/after the crew and sessions within the entrypoint, including cancellation.
The API monitor waits for worker cancellation before reading usage. A different
durable terminal writer remains authoritative; its snapshot is not overwritten.
Use `crew:flow_usage()` for the wider process-local CLI flow view.

Intents and owner-death reconciliation have no trustworthy terminal checkpoint:
their unavailable snapshot has zero **observed** requests and null counts, not
evidence of zero cost. SQL schema upgrades add this explicit unavailable marker
to rows without a checkpoint; they never infer receipts from old integer columns.
Old columns are left untouched, but are no longer read or written. Old JSON run
records and old task-result payloads without `usage` are unsupported and fail
decoding; export/archive them before upgrading. No legacy result adapter exists.
Conversation/dialog records require the same checked `usage` field. SQL migration
marks older session checkpoints unavailable; old JSON session shapes without it
are unsupported. A successful save commits usage and transcript together under
the existing revision check (and HTTP idempotency transaction when applicable).
In-flight snapshots are rejected. Resuming restores usage without charging that
history to the new run/flow. Resetting transcript history does not reset usage.

Session durability is **checkpoint-based**, not a billing journal: a failed or
cancelled turn retains its receipt in the live handle; the next successful save
includes it even though its transcript was rolled back. Eviction/process death
before that save loses those unsaved receipts. `/history` and list summaries show
the last durable checkpoint, not a guarantee of all provider activity since it.
CLI `/usage` and Lua session methods show the live handle; successful HTTP
message responses include its `usage` snapshot. No execution recovery is implied.

## Lua snapshots

| Method | Scope |
|---|---|
| `crew:usage()` | Latest run that entered execution; `nil` before any such run |
| `crew:flow_usage()` | Inclusive enclosing VM/caller scope, including all its crews and sessions |
| `conversation:usage()` | Restored session checkpoint plus calls observed by this handle |
| `dialog:usage()` | Restored session checkpoint plus calls observed by this handle |

Each call returns a detached snapshot, not a live mutable view. A new run replaces
the crew's last-run view; earlier returned snapshots stay unchanged. Nested calls
are already included in ancestors: never add `crew:usage()` to `crew:flow_usage()`.
Explicit conversation caller scopes charge the chosen caller instead of the
creation flow, while also updating the independent session observer. Shared
ancestors are deduplicated; bounded scope graphs permit 64 levels and 128 nodes.
Session lifetime totals and current-run totals overlap only for new calls: never
add them together. History restored from storage is not dispatched work.

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
counters per node, at most 64 child levels and 128 ancestry nodes. Its
attempt guard settles exactly once on completion or drop, including cancellation
of a polled async request. Clones share a scope; separately created trackers do
not. `child()` creates a disjoint child view and updates its inclusive ancestors
atomically, with globally ordered locks and deduplicated observers. A scope-limit failure rejects the new scope
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
credits and monetary reconciliation. Partial or unavailable usage remains
explicit in CLI/Lua/HTTP/store results.

## Provider metrics

Built-in transports record one receipt per dispatch, including errors and
cancelled streams, even without an execution tracker. Runtime wrappers do not
count those receipts again. Custom providers use wrapper telemetry unless they
declare `records_usage_metrics()` and own it themselves.

`ironcrew_provider_tokens_total` contains known lower bounds, with fixed `type`
labels `prompt`, `completion`, `total`, `cached`, `cache_write`, `reasoning`.
Pair it with `ironcrew_provider_usage_incomplete_fields_total` (same labels) and
`ironcrew_provider_usage_receipts_total` (`provider`, `coverage`) to distinguish
unknown details from known zero. These process-local operational counters are
not lossless billing records; use checked snapshots for exact unsigned counts.
