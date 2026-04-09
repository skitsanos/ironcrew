# IronCrew Memory & Enterprise Remediation Plan

This plan addresses all findings in `review.md`. All file paths and line
numbers have been verified against the current codebase. The plan is organized
into four tiers by severity × effort, each intended as a separate release.

---

## Tier 1 — Correctness bugs (v2.4.1, patch release)

Four small, discrete fixes. Total effort: **~2 hours**. No architectural
changes, no new env vars, no API surface changes. Safe to ship as a patch.

### 1.1 Fix `total_tokens = 0` in `RunComplete` SSE event

**File:** `src/api/handlers.rs:128`

```rust
// Current (always 0):
total_tokens: response.results.iter().map(|_| 0u32).sum(),

// Fix: sum the actual token_usage.total_tokens from each task
total_tokens: response
    .results
    .iter()
    .filter_map(|r| r.token_usage.as_ref().map(|u| u.total_tokens))
    .sum(),
```

**Blast radius:** zero — fixes a bug in the SSE payload, no behavior change.

**Test:** add a unit test asserting non-zero totals when any task has token usage.

---

### 1.2 Fix Unicode panic in SSE truncation

**File:** `src/api/handlers.rs:404-413` and `421-429`

Both `output[..max]` and `content[..max]` are byte slices that panic if `max`
lands inside a multi-byte UTF-8 codepoint.

Add a helper and use it in both places:

```rust
/// Truncate a string to at most `max` bytes without breaking a UTF-8 codepoint.
fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    // Walk back from `max` to the last valid char boundary
    let mut boundary = max;
    while boundary > 0 && !s.is_char_boundary(boundary) {
        boundary -= 1;
    }
    &s[..boundary]
}
```

Replace `&output[..max]` → `truncate_utf8(output, max)` and same for content.

**Blast radius:** zero behavior change for ASCII-only content; no panic on
Unicode. Add a test with emoji + CJK text.

---

### 1.3 Log persistent memory save failures

**File:** `src/engine/orchestrator.rs:745-746`

```rust
// Current:
crew.memory.save().await.ok();

// Fix:
if let Err(e) = crew.memory.save().await {
    tracing::error!("Failed to persist memory at end of run: {}", e);
    // Also emit a log event so REST API subscribers see it
    crew.eventbus.emit(CrewEvent::Log {
        level: "error".into(),
        message: format!("Memory persistence failed: {}", e),
    });
}
```

**Blast radius:** zero — adds observability, no behavior change.

---

### 1.4 Only persist new task results to memory per phase

**File:** `src/engine/orchestrator.rs:709-718`

```rust
// Current: loops over the ENTIRE results map every phase
for (task_name, result) in &results {
    if result.success {
        crew.memory.set(format!("task:{}", task_name), value).await;
    }
}

// Fix: track which tasks have already been persisted
// Add to run_crew() before the phase loop:
let mut persisted_to_memory: HashSet<String> = HashSet::new();

// Then in the phase loop:
for (task_name, result) in &results {
    if result.success && !persisted_to_memory.contains(task_name) {
        // ... save ...
        persisted_to_memory.insert(task_name.clone());
    }
}
```

**Blast radius:** zero behavior change, reduces CPU and allocation churn on
multi-phase runs. No user-visible difference.

**Summary of Tier 1:** 4 files touched, ~30 lines of changes, 1 new helper
function, 2 new unit tests. Ship as **v2.4.1**.

---

## Tier 2 — Default budgets (v2.5.0, minor release)

Five enterprise-hardening fixes that add default limits and new env vars.
Total effort: **~1 day**. No breaking changes — existing crews still work
because defaults are set to values that won't affect normal usage.

### 2.1 `LuaConversation` default `max_history`

**File:** `src/lua/conversation.rs:51-55, 111-129, 164-229`

Currently `max_history: Option<usize>` defaults to `None` (unbounded). Change
the default to a safe value, but keep the option to override.

```rust
// In src/lua/conversation.rs build_conversation():
let max_history: Option<usize> = table.get("max_history").ok()
    .or_else(|| {
        std::env::var("IRONCREW_CONVERSATION_MAX_HISTORY")
            .ok()
            .and_then(|v| v.parse().ok())
    })
    .or(Some(50));  // NEW: safe default
```

**New env var:** `IRONCREW_CONVERSATION_MAX_HISTORY` (default: 50)

**Blast radius:** minor — conversations running past 50 messages will start
losing oldest context. The existing `max_history` option is still honored and
users can pass a larger value or explicitly set `max_history = nil` (would
need to add a new sentinel or a separate "unbounded" flag to disable entirely).

**Decision needed:** should we allow explicit opt-out via `max_history = 0`
meaning unbounded, or force a cap in all cases?
**My recommendation:** support `max_history = 0` as "unbounded" for backward
compat, document that the default is 50.

---

### 2.2 `AgentDialog` transcript cap (not just prompt cap)

**File:** `src/lua/dialog.rs:66-78` (struct), `267-286` (build_messages)

Currently `max_history` only trims the temporary `messages` vec inside
`build_messages()` — the stored `transcript` keeps growing up to `max_turns`.

Add a method `enforce_transcript_cap()` that trims the stored transcript the
same way conversations do:

```rust
async fn enforce_transcript_cap(&self) {
    let Some(cap) = self.max_history else { return };
    let mut transcript = self.transcript.lock().await;
    if transcript.len() > cap {
        let excess = transcript.len() - cap;
        transcript.drain(..excess);
    }
}
```

Call it after each `execute_turn()` completion, before emitting events.

**New env var:** `IRONCREW_DIALOG_MAX_HISTORY` (default: 100)

**Blast radius:** users relying on full transcript retention via
`dialog:transcript()` would see older turns dropped. Mitigation: only trim
transcripts that exceed the cap; warn in debug logs when trimming occurs.

---

### 2.3 `MessageBus` per-agent queue caps + pending broadcasts cap

**File:** `src/engine/messagebus.rs:52-58, 75-128`

Add bounded per-agent queues and cap `pending_broadcasts`.

```rust
pub struct MessageBus {
    queues: Arc<RwLock<HashMap<String, VecDeque<Arc<Message>>>>>,
    history: Arc<RwLock<VecDeque<Arc<Message>>>>,
    pending_broadcasts: Arc<RwLock<VecDeque<Arc<Message>>>>,  // was Vec
    max_queue_depth: usize,
    max_pending_broadcasts: usize,
}

impl MessageBus {
    pub fn new() -> Self {
        let max_queue_depth = std::env::var("IRONCREW_MESSAGEBUS_QUEUE_DEPTH")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);
        let max_pending_broadcasts = std::env::var("IRONCREW_MESSAGEBUS_PENDING_CAP")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(500);
        // ...
    }

    pub async fn send(&self, message: Message) {
        // ... existing logic, but when pushing to a queue:
        let queue = queues.entry(name).or_default();
        queue.push_back(message);
        while queue.len() > self.max_queue_depth {
            tracing::warn!(
                "MessageBus: dropping oldest message from queue '{}' (depth {})",
                name, queue.len()
            );
            queue.pop_front();
        }
    }
}
```

**New env vars:**
- `IRONCREW_MESSAGEBUS_QUEUE_DEPTH` (default: 1000 per agent)
- `IRONCREW_MESSAGEBUS_PENDING_CAP` (default: 500)

**Blast radius:** in pathological cases where one agent produces faster than
another consumes, the oldest messages are dropped (warn logged). For normal
workflows the default of 1000 is well above real-world need.

---

### 2.4 EventBus payload truncation at emit time (not just SSE serialization)

**File:** `src/engine/eventbus.rs:31-224`, `src/engine/orchestrator.rs:147-167`

The review is right that the replay buffer stores full event payloads, and
SSE truncation happens only at response serialization. This means large task
outputs are retained multiple times: in `results`, in the run record, and in
the replay buffer (up to 1000 events).

**Simpler approach than the review's "store IDs only" suggestion:** truncate
large text fields *before* calling `eventbus.emit()`. The full output still
lives in the run record (which is the source of truth). The replay buffer
only keeps a summary.

Add a helper in `eventbus.rs`:

```rust
impl EventBus {
    fn truncate_output(s: &str) -> String {
        let max = std::env::var("IRONCREW_EVENT_PAYLOAD_MAX")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8192); // 8KB per event payload by default
        if s.len() <= max {
            return s.to_string();
        }
        let boundary = (0..=max).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0);
        format!("{}... [truncated, {} total bytes]", &s[..boundary], s.len())
    }
}
```

Then in `orchestrator.rs process_phase_results()` where TaskCompleted is emitted:

```rust
crew.eventbus.emit(CrewEvent::TaskCompleted {
    task: task_name.clone(),
    agent: agent_name.clone(),
    duration_ms,
    success: true,
    output: EventBus::truncate_output(&out),  // truncate before emit
    token_usage: ...
});
```

Same for `TaskThinking`, `ConversationTurn`, `DialogTurn`, `CollaborationTurn`.

**New env var:** `IRONCREW_EVENT_PAYLOAD_MAX` (default: 8192 bytes)

**Blast radius:** REST API subscribers see truncated output in SSE streams
(with a clear `[truncated, N total bytes]` marker). Full output is still
available via `/flows/{flow}/runs/{id}` which reads from the persisted
RunRecord. This is a **deliberate tradeoff** that keeps replay memory bounded.

The existing `IRONCREW_SSE_OUTPUT_MAX_CHARS` env var can be deprecated (or
kept as an alias). Deprecation note in docs.

**Decision needed:** should SSE consumers expect full content or truncated?
**My recommendation:** truncate in events (operators care more about
notification than full content), provide a separate endpoint to fetch
full task output by run_id + task_name for consumers that need it.

---

### 2.5 EventBus replay byte budget

**File:** `src/engine/eventbus.rs:115-163`

Currently the replay buffer caps by count (`IRONCREW_MAX_EVENTS`, default
1000). Add a byte budget as an additional cap.

```rust
pub struct EventBus {
    sender: Arc<broadcast::Sender<Arc<CrewEvent>>>,
    history: Arc<RwLock<VecDeque<Arc<CrewEvent>>>>,
    max_replay: usize,
    max_replay_bytes: usize,    // NEW
    current_bytes: Arc<RwLock<usize>>,  // tracked separately
}

impl EventBus {
    pub fn emit(&self, event: CrewEvent) {
        let event = Arc::new(event);
        let size = estimate_event_size(&event);
        // ... push and evict until under both caps
    }
}

fn estimate_event_size(event: &CrewEvent) -> usize {
    // Rough byte estimate via serde_json::to_string
    serde_json::to_string(event).map(|s| s.len()).unwrap_or(256)
}
```

**New env var:** `IRONCREW_EVENT_REPLAY_MAX_BYTES` (default: 4 MB)

**Blast radius:** zero visible change unless very large events were being
stored, in which case older events are dropped (same as the existing count
cap).

---

**Summary of Tier 2:** 4 files touched, ~150 lines of changes, 5 new env vars.
Ship as **v2.5.0** (minor bump because new env vars and default behavior
change for conversations/dialogs).

---

## Tier 3 — Tool hardening (v2.6.0)

Five tool-level fixes. Total effort: **~1 day**. Each tool gets bounded input
and/or output.

### 3.1 `http_request`: stream body with byte cap when `Content-Length` is missing

**File:** `src/tools/http_request.rs:165-188`

Current code checks `content_length()` but then calls `resp.text()` which
reads the full body regardless. Fix: use `bytes_stream()` with a running
byte counter, abort when over `max_response_size`.

```rust
use futures::StreamExt;

let mut body = Vec::with_capacity(4096);
let mut stream = resp.bytes_stream();
while let Some(chunk) = stream.next().await {
    let chunk = chunk.map_err(|e| IronCrewError::ToolExecution {
        tool: "http_request".into(),
        message: format!("Failed to read response: {e}"),
    })?;
    if body.len() + chunk.len() > max_response_size as usize {
        return Err(IronCrewError::ToolExecution {
            tool: "http_request".into(),
            message: format!(
                "Response exceeded max size of {} bytes (stream aborted)",
                max_response_size
            ),
        });
    }
    body.extend_from_slice(&chunk);
}
let body_text = String::from_utf8_lossy(&body).to_string();
```

**Env var:** reuses existing `IRONCREW_MAX_RESPONSE_SIZE` (default 50MB).

**Blast radius:** servers that omit `Content-Length` but send large bodies
will now be rejected. This is the desired behavior.

---

### 3.2 `web_scrape`: cap raw HTML before DOM parse

**File:** `src/tools/web_scrape.rs:95-132`

Same streaming pattern as 3.1, plus limit raw HTML bytes before `Html::parse_document`.

```rust
let max_html_bytes: usize = std::env::var("IRONCREW_WEB_SCRAPE_MAX_BYTES")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(2 * 1024 * 1024); // 2 MB default
// ... stream with byte cap as in 3.1 ...
```

**New env var:** `IRONCREW_WEB_SCRAPE_MAX_BYTES` (default: 2 MB)

**Blast radius:** sites with >2MB HTML (rare) get truncated. Output char cap
of 10000 already exists — this is an upstream guard.

---

### 3.3 `file_read`: max file size

**File:** `src/tools/file_read.rs:71-86`

```rust
// Check size before reading
let metadata = tokio::fs::metadata(&validated).await
    .map_err(|e| IronCrewError::ToolExecution { ... })?;

let max_file_size: u64 = std::env::var("IRONCREW_FILE_READ_MAX_BYTES")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(10 * 1024 * 1024); // 10 MB

if metadata.len() > max_file_size {
    return Err(IronCrewError::ToolExecution {
        tool: "file_read".into(),
        message: format!(
            "File '{}' is {} bytes, exceeds limit of {}",
            path, metadata.len(), max_file_size
        ),
    });
}

tokio::fs::read_to_string(&validated).await...
```

**New env var:** `IRONCREW_FILE_READ_MAX_BYTES` (default: 10 MB)

---

### 3.4 `file_read_glob`: file count and total byte caps

**File:** `src/tools/file_read_glob.rs:47-118`

Add both a max file count and a running total byte budget. When either is
exceeded, return what was collected so far with a truncation marker.

```rust
let max_files: usize = std::env::var("IRONCREW_GLOB_MAX_FILES")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(500);

let max_total_bytes: u64 = std::env::var("IRONCREW_GLOB_MAX_BYTES")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(50 * 1024 * 1024); // 50 MB

let mut total_bytes: u64 = 0;
let mut truncated = false;

for entry in entries {
    if results.len() >= max_files {
        truncated = true;
        break;
    }
    // ... read file ...
    total_bytes += content.len() as u64;
    if total_bytes > max_total_bytes {
        truncated = true;
        break;
    }
    results.push(...);
}

let mut output = json!({
    "files": results,
    "file_count": results.len(),
    "total_bytes": total_bytes,
});
if truncated {
    output["truncated"] = json!(true);
}
```

**New env vars:**
- `IRONCREW_GLOB_MAX_FILES` (default: 500)
- `IRONCREW_GLOB_MAX_BYTES` (default: 50 MB)

**Blast radius:** workflows reading >500 files or >50MB will get a truncated
result with an explicit flag. Output shape changes from a bare array to an
object with `files`/`truncated` fields — **this is a breaking change** for
existing consumers.

**Decision needed:** keep the bare-array shape and add a separate out-of-band
metadata mechanism, or break the shape?
**My recommendation:** break the shape in v2.6.0 (major-ish behavior change
gets a minor version bump). Document clearly in release notes.

---

### 3.5 `shell`: stdout/stderr capture caps

**File:** `src/tools/shell.rs:58-79`

Current code uses `Command.output()` which buffers the full stdout and stderr.
Replace with `spawn()` + piped output + bounded reading:

```rust
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::Command;

let max_output_bytes: usize = std::env::var("IRONCREW_SHELL_MAX_OUTPUT_BYTES")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(1024 * 1024); // 1 MB each for stdout/stderr

let mut child = Command::new("sh")
    .arg("-c").arg(command)
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .spawn()?;

let mut stdout = Vec::with_capacity(4096);
let mut stderr = Vec::with_capacity(4096);
let stdout_pipe = child.stdout.take().unwrap();
let stderr_pipe = child.stderr.take().unwrap();

let (stdout_result, stderr_result) = tokio::join!(
    read_bounded(stdout_pipe, &mut stdout, max_output_bytes),
    read_bounded(stderr_pipe, &mut stderr, max_output_bytes),
);
// ... wait on child, build output ...
```

With a helper `read_bounded(mut reader, buf, max)` that reads up to `max`
bytes and then discards the rest (or kills the process — operator choice).

**New env var:** `IRONCREW_SHELL_MAX_OUTPUT_BYTES` (default: 1 MB per stream)

**Blast radius:** shell commands producing >1 MB of output get truncated.
This tool is already opt-in via `IRONCREW_ALLOW_SHELL`, so the user is
accepting risk.

---

**Summary of Tier 3:** 5 files touched, ~300 lines of changes, 6 new env vars,
one deliberate breaking change in `file_read_glob` output shape. Ship as
**v2.6.0**.

---

## Tier 4 — Run history API redesign (v2.7.0 or later)

This is the largest architectural change and benefits from its own release
and design pass.

### 4.1 Lightweight `RunSummary` DTO

Create a new struct that excludes `task_results`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummary {
    pub run_id: String,
    pub flow_name: String,
    pub status: RunStatus,
    pub started_at: String,
    pub finished_at: String,
    pub duration_ms: u64,
    pub agent_count: usize,
    pub task_count: usize,
    pub total_tokens: u32,
    pub cached_tokens: u32,
    pub tags: Vec<String>,
    // NO task_results — fetched on demand
}
```

### 4.2 Extend `StateStore` trait with `list_runs_summary`

```rust
#[async_trait]
pub trait StateStore: Send + Sync {
    // Existing methods...
    async fn save_run(&self, record: &RunRecord) -> Result<String>;
    async fn get_run(&self, run_id: &str) -> Result<RunRecord>;

    // New:
    async fn list_runs_summary(
        &self,
        limit: usize,
        offset: usize,
        filter: ListRunsFilter,
    ) -> Result<Vec<RunSummary>>;
    async fn count_runs(&self, filter: &ListRunsFilter) -> Result<u64>;
}

pub struct ListRunsFilter {
    pub flow_name: Option<String>,
    pub status: Option<RunStatus>,
    pub tag: Option<String>,
    pub since: Option<String>,  // RFC3339
}
```

Implementations in all three backends:
- `JsonFileStore`: read directory, sort by mtime, paginate in memory
- `SqliteStore`: `SELECT ... LIMIT ? OFFSET ?` without the task_results column
- `PostgresStore`: same, with JSONB field exclusion

### 4.3 New REST API shape

```
GET /flows/{flow}/runs?limit=20&offset=0&status=success
→ returns { runs: [RunSummary], total: N, limit, offset }

GET /flows/{flow}/runs/{run_id}
→ returns full RunRecord (unchanged)
```

Query params: `limit` (default 20, max 100), `offset` (default 0), `status`,
`tag`, `since`.

### 4.4 Deprecate and remove unbounded endpoint

The current `/flows/{flow}/runs` (returns all runs with full task results) is
kept for backward compat in v2.7.0 but marked deprecated in docs. Removed in
v3.0.0.

**Effort:** **2-3 days** for a careful redesign. Touches all three storage
backends, the REST API, the CLI `runs` command, and all relevant docs.

**Summary of Tier 4:** 8-10 files touched, ~400 lines of changes, new env var
`IRONCREW_RUNS_DEFAULT_LIMIT` (default 20). Ship as **v2.7.0**.

---

## Release sequencing

| Version | Tier | Scope | Effort | Type |
|---------|------|-------|--------|------|
| **v2.4.1** | Tier 1 | 4 correctness bugs | ~2 hours | Patch |
| **v2.5.0** | Tier 2 | Default budgets | ~1 day | Minor (new env vars, behavior changes) |
| **v2.6.0** | Tier 3 | Tool hardening | ~1 day | Minor (breaking shape in file_read_glob) |
| **v2.7.0** | Tier 4 | Run history pagination | ~2-3 days | Minor (new endpoints, deprecations) |

Total across all tiers: **~1 week of focused work** for full enterprise-grade
readiness.

---

## Open design questions

Before I start implementing, three decisions need your input:

1. **Tier 2.1 — Conversation default `max_history`:** should we allow
   `max_history = 0` to mean "unbounded" (backward compat), or force a cap in
   all cases? **My recommendation: support `max_history = 0` for unbounded.**

2. **Tier 2.4 — Event payload truncation:** should SSE consumers expect full
   content in events, or is truncation acceptable? **My recommendation:
   truncate events, consumers fetch full content via the runs endpoint.**

3. **Tier 3.4 — `file_read_glob` output shape:** keep the bare JSON array
   (add out-of-band metadata), or break the shape to include
   `{files, truncated}`? **My recommendation: break the shape in v2.6.0.**

---

## Notes on the review's specific suggestions I'm NOT adopting

### "Store only identifiers in replay history and fetch payloads lazily"
(Finding #4)

I'm choosing **truncation before emit** instead. Lazy fetch would require a
separate payload store, new lookup paths, and more plumbing. Simple
truncation achieves the same memory goal with a fraction of the complexity.
The full payloads are already persisted in the run record, which is the
source of truth.

### "Token-based cap, not only message-count cap" (Finding #2)

Token-based caps require running a tokenizer (or estimating) on every
message, which adds CPU overhead and a tokenizer dependency. Message-count
cap + payload truncation is a pragmatic approximation. We can revisit if
real workloads show it's insufficient.

### "Summarize older turns into a compact system or memory entry"
(Findings #2, #3)

Summarization requires an LLM call, which introduces cost, latency, and a
new failure mode. For a first pass, drop-oldest is simpler and predictable.
Summarization is a future enhancement (v3.0+).
