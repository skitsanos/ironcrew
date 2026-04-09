# IronCrew Memory and Enterprise Review

## Executive Summary

I did not find evidence of a classical Rust memory leak in the core codebase, meaning I did not find production code using `Box::leak`, `mem::forget`, reference-count cycles with `Rc`/`Weak`, or similar ownership bugs that would permanently orphan memory. That is the good news.

The less comfortable conclusion is that the project still has several leak-like retention paths that can make RAM usage grow continuously in a long-lived Cloud deployment. In practice, those will feel like memory leaks to operators because memory is retained longer than needed and some paths are effectively unbounded.

The most important risks are:

- Unbounded agent message queues in `MessageBus`
- Unbounded conversation history by default
- Dialog transcripts that keep growing even when prompt history is capped
- Event replay buffers that retain full task outputs and reasoning
- Tools that fully buffer external responses, command output, and files in memory
- Run-history endpoints that materialize complete historical payloads without pagination

I also found a few correctness and enterprise-hardening issues:

- `RunComplete.total_tokens` is always reported as `0` in the API flow runner
- SSE truncation can panic on non-ASCII text because it slices UTF-8 strings by byte offset
- Persistent memory save errors are silently ignored

## Verification Performed

- Static code review of the runtime, API, memory, event, tool, and storage layers
- `cargo test`
- `cargo clippy --all-targets -- -D warnings`

Both the test suite and Clippy passed at the time of review.

## Findings

### 1. High: `MessageBus` queues can grow without bound

**Finding**

`MessageBus` caps history at 500 entries, but it does not cap per-agent queues or `pending_broadcasts`.

**Why this matters**

If one agent sends faster than another consumes, memory grows indefinitely. In a service process, this is operationally equivalent to a memory leak. The same applies to broadcasts sent before registration: they accumulate in `pending_broadcasts` with no limit.

**Evidence**

- `src/engine/messagebus.rs:52-58`
- `src/engine/messagebus.rs:75-111`
- `src/engine/messagebus.rs:114-128`

`queues` is `HashMap<String, VecDeque<Arc<Message>>>` and `pending_broadcasts` is `Vec<Arc<Message>>`, but only `history` is bounded.

**Possible ways to resolve**

- Add a per-agent queue limit by message count and by total bytes
- Add a separate cap for `pending_broadcasts`
- Drop oldest entries when over budget and emit metrics/log warnings
- Add TTL-based expiry for stale messages
- Consider backpressure or a `send` failure when a queue is saturated

### 2. High: `LuaConversation` history is unbounded unless the caller opts into `max_history`

**Finding**

Conversation state stores every user message, assistant message, tool-call request, and tool result. The cap is optional and defaults to `None`.

**Why this matters**

For long-running interactive sessions this produces linear memory growth with no default guardrail. Since the stored message list is also cloned for every request, the cost is not just retained RAM but also repeated allocation and copy pressure.

**Evidence**

- `src/lua/conversation.rs:51-55`
- `src/lua/conversation.rs:111-129`
- `src/lua/conversation.rs:164-229`
- `src/lua/conversation.rs:297-311`
- `src/lua/conversation.rs:425-435`

`enforce_history_cap()` is a no-op when `max_history` is not configured.

**Possible ways to resolve**

- Set a safe default `max_history`
- Support a token-based cap, not only message-count cap
- Summarize older turns into a compact system or memory entry
- Add a hard cap for stored tool results, since those can be much larger than normal chat turns

### 3. High: `AgentDialog` keeps the full transcript even when prompt history is capped

**Finding**

The dialog prompt builder honors `max_history`, but the underlying transcript remains unbounded up to `max_turns`, and the full transcript is cloned on `run_all()` and `transcript()`.

**Why this matters**

This means memory growth is controlled for prompt construction only, not for retained process state. Large turns, especially with reasoning attached, will accumulate in RAM for the lifetime of the dialog object.

**Evidence**

- `src/lua/dialog.rs:70-78`
- `src/lua/dialog.rs:166-191`
- `src/lua/dialog.rs:267-286`
- `src/lua/dialog.rs:398-410`
- `src/lua/dialog.rs:511-520`
- `src/lua/dialog.rs:558-562`
- `src/lua/dialog.rs:683-687`

`max_history` only trims the temporary `messages` vector in `build_messages()`. It does not trim `transcript`.

**Possible ways to resolve**

- Apply the same cap to the stored transcript, not just to prompt assembly
- Add a transcript byte budget and reasoning byte budget
- Keep a summarized transcript plus a short ring buffer of recent raw turns
- Avoid cloning the entire transcript for every read path when a borrowed/streamed response would do

### 4. High: The event system retains full outputs and reasoning in memory; SSE truncation happens too late

**Finding**

Task completion, thinking, collaboration, conversation, and dialog events all carry full strings. `EventBus` stores those events in a replay buffer. Truncation only happens in the HTTP SSE response layer, after the full event is already retained in memory.

**Why this matters**

A run with large task outputs can be stored multiple times at once:

- in `results`
- in memory entries
- in the event replay buffer
- in the final run record

That creates multiplicative RAM pressure. In Cloud, this is one of the most expensive patterns in the codebase.

**Evidence**

- `src/engine/eventbus.rs:31-57`
- `src/engine/eventbus.rs:180-224`
- `src/engine/orchestrator.rs:147-167`
- `src/api/handlers.rs:392-432`

The replay buffer stores the original `CrewEvent`; the truncation helper only affects SSE serialization.

**Possible ways to resolve**

- Truncate or summarize large event payloads before calling `emit()`
- Separate “full artifact” storage from “event notification” storage
- Store only identifiers in replay history and fetch payloads lazily
- Add byte-based replay limits, not only event-count limits
- Consider disabling storage of reasoning by default in production mode

### 5. High: Several built-in tools fully buffer unbounded input/output in memory

**Finding**

Multiple tools read entire payloads into memory before returning results.

**Why this matters**

This is a direct RAM-risk surface because tools are one of the easiest paths for an LLM or a user workflow to touch very large external data. Several of these paths can exceed the intended limits.

**Evidence**

- `src/tools/http_request.rs:165-188`
- `src/tools/web_scrape.rs:95-123`
- `src/tools/file_read.rs:79-85`
- `src/tools/file_read_glob.rs:69-117`
- `src/tools/shell.rs:58-77`

Specific issues:

- `http_request` checks `Content-Length`, but if the header is missing it still reads the entire body with `resp.text()`
- `web_scrape` reads the entire HTML document, then truncates only after extraction
- `file_read` has no size guard
- `file_read_glob` can read an arbitrary number of files and aggregate all contents into one JSON array
- `shell` captures all stdout/stderr with `.output()`

**Possible ways to resolve**

- Enforce hard byte caps while streaming, not after full buffering
- Add file count and total-byte limits to `file_read_glob`
- Add maximum stdout/stderr capture sizes to `shell`
- Add maximum file size to `file_read`
- In `web_scrape`, stream and cap bytes before full DOM parse
- Return partial content plus truncation metadata instead of full payloads

### 6. Medium-High: Run history listing materializes full runs with full task outputs and no pagination

**Finding**

The run-history listing path loads complete `RunRecord`s, including every task result, and the API returns the full list in a single response.

**Why this matters**

This is a RAM and latency risk that gets worse over time. Historical data size grows with both number of runs and size of task outputs. Listing history should not require loading all outputs for every stored run.

**Evidence**

- JSON backend: `src/engine/run_history.rs:91-110`
- SQLite backend: `src/engine/sqlite_store.rs:131-189`
- PostgreSQL backend: `src/engine/postgres_store.rs:237-258`
- API surface: `src/api/handlers.rs:513-529`

All of these paths return full `RunRecord`s instead of light summaries.

**Possible ways to resolve**

- Add pagination (`limit`, `offset` or cursor)
- Split run summary metadata from detailed task outputs
- Expose a lightweight `list_runs` DTO and reserve full task results for `get_run`
- Add retention and archival policies for old runs

### 7. Medium: Successful task results are re-written into memory after every phase

**Finding**

After each phase, the orchestrator iterates over the entire `results` map and re-saves every successful task into `MemoryStore`, not only the new ones from that phase.

**Why this matters**

This is not a memory leak, but it is wasted work and causes repeated cloning of large outputs, repeated token estimation, repeated revision bumps, and repeated eviction checks. On large workflows it increases CPU and allocation churn for no benefit.

**Evidence**

- `src/engine/orchestrator.rs:709-718`

**Possible ways to resolve**

- Store only the results produced in the current phase
- Track whether a task has already been materialized into memory
- Make task-result persistence into memory configurable
- Consider storing a compact summary instead of the full output

### 8. Medium: `RunComplete.total_tokens` is always zero in the API runner

**Finding**

The API emits `RunComplete.total_tokens` using `response.results.iter().map(|_| 0u32).sum()`, which always evaluates to `0`.

**Why this matters**

This breaks observability and cost accounting. In a Cloud deployment, token usage is one of the most important metrics to get right.

**Evidence**

- `src/api/handlers.rs:123-129`

**Possible ways to resolve**

- Return aggregated token usage from the execution path and use it directly here
- Or sum per-task token usage from the stored run record before emitting `RunComplete`
- Add tests asserting non-zero totals when token usage is present

### 9. Medium: SSE truncation can panic on Unicode boundaries

**Finding**

The SSE truncation logic slices strings with `&output[..max]` and `&content[..max]`. `max` is treated like a safe character index, but Rust string slicing uses byte offsets and requires UTF-8 boundaries.

**Why this matters**

If `IRONCREW_SSE_OUTPUT_MAX_CHARS` is set and the cut point lands inside a multi-byte code point, the API can panic during event streaming.

**Evidence**

- `src/api/handlers.rs:404-413`
- `src/api/handlers.rs:421-429`

**Possible ways to resolve**

- Truncate by `char_indices()` or use a UTF-8-safe helper
- Keep a byte limit and adjust to the previous valid boundary
- Add tests with emoji and non-Latin text

### 10. Medium: Persistent memory save failures are silently ignored

**Finding**

When the crew finishes, memory persistence errors are dropped with `.ok()`.

**Why this matters**

This is not a RAM leak, but it weakens reliability. A production system can appear healthy while silently losing persistent memory state.

**Evidence**

- `src/engine/orchestrator.rs:745-746`

**Possible ways to resolve**

- Surface the error in logs at minimum
- Optionally fail the run when persistence is configured and save fails
- Emit a dedicated event or metric for persistence failures

## Additional Enterprise-Grade Improvement Opportunities

These are not the top memory findings, but they are worth addressing.

### A. Add explicit production budgets

The codebase would benefit from one central configuration surface for:

- max event replay bytes
- max tool response bytes
- max conversation/dialog history tokens
- max run-history page size
- max message-queue depth

Today, some limits exist, but they are inconsistent and mostly count-based rather than byte-based.

### B. Separate “interactive convenience” from “server mode”

The current defaults are reasonable for local experimentation, but not for a multitenant or cost-sensitive Cloud runtime. A dedicated production profile should disable or aggressively cap:

- full reasoning retention
- full event replay payloads
- unrestricted conversation history
- unrestricted tool output capture

### C. Add memory-focused tests and load tests

The existing test suite is solid for correctness, but it does not appear to include stress tests for retained-memory behavior. Useful additions would be:

- message-queue growth tests
- long conversation retention tests
- long dialog retention tests
- large-response tool tests
- run-history listing under many large runs

## Bottom Line

The project does not currently show signs of a classic Rust ownership leak. However, it does have several unbounded retention patterns that will produce rising RAM usage in real Cloud workloads and can become expensive quickly.

If the goal is enterprise-grade readiness, I would prioritize fixes in this order:

1. Bound `MessageBus` queues and pending broadcasts
2. Add safe default caps for `LuaConversation` and `AgentDialog`
3. Reduce event payload retention and truncate before storage
4. Stream and cap tool outputs instead of buffering them fully
5. Redesign run-history listing to use summaries and pagination

