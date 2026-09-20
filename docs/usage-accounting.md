# Usage accounting (IC-046, in progress)

This page describes the Rust `ironcrew::usage` contract and the built-in HTTP
provider capture layer. Rust callers can explicitly attach a shared tracker to
`ChatRequest.usage_tracker`; OpenAI Chat, Responses and Anthropic then retain
checked receipts independently of their output return value.

**Automatic run/task/conversation scope propagation, `ChatResponse.usage`,
task results, CLI, Lua, HTTP events and stored run/session records have not yet
migrated.** Those still have the gaps tracked by [IC-046](issues/IC-046.md).
The old usage fields are not a fallback source for the new tracker. Do not use
transport tests as proof of end-to-end billing observability or an IC-047 budget.

## Request-scoped HTTP capture

Rust callers set `request.usage_tracker = Some(tracker.clone())` before calling
any built-in provider's `chat`, `chat_with_tools` or `chat_stream`, and read
`tracker.snapshot()` afterward, including after an error. Sharing that tracker
across retries or concurrent calls includes each dispatched attempt once.
`Agent::chat_request` leaves the scope unset; callers must choose its ownership
explicitly. The scope is never serialized into the provider request or retained
globally. Custom `LlmProvider` implementations do not automatically participate.

An attempt starts after local validation and rate-limit waiting, immediately
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
`UsageCounts` uses unsigned 64-bit integers and explicit `None`/JSON `null` for
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

The JSON numbers preserve the Rust `u64` range. Consumers must use a lossless
integer decoder for values above JavaScript's safe-integer range. Public Lua,
HTTP and SQL encoding choices remain part of the pending runtime integration;
the old signed PostgreSQL `INTEGER` columns cannot store this full range.

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

`UsageTracker` is a process-local, constant-space shared accounting scope. Its
attempt guard settles exactly once on completion or drop, including cancellation
of a polled async request. Clones share a scope; separately created trackers do
not. Retries and delegated calls must use separate guards on the same run scope.
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

1. Propagate scopes automatically through task retries, conversations and
   nested/delegated execution, including custom providers. Replace the old
   response/task usage types rather than adding a compatibility fallback.
   Prove actual executor ownership with mock providers; the loopback HTTP tests
   cover transport capture, not automatic runtime propagation.
2. Carry the same coverage contract through Lua, CLI, HTTP/events, run/session
   persistence and JSON/SQLite/PostgreSQL. Preserve owner fencing and terminal
   compare-and-set behavior; select lossless storage/wire representations.
3. Run durable-backend acceptance against disposable PostgreSQL 15 and the
   complete affected gates before resolving IC-046 or implementing IC-047.
