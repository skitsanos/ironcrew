# Storage Backends

IronCrew uses a pluggable storage system for persisting run records, powered by
the `StateStore` trait. Its lifetime depends on the mode:

- **`ironcrew serve`** — a **single store instance is bootstrapped once at
  server startup** (see `cmd_serve` in `src/cli/server.rs`) and shared across
  every request handler. Postgres migrations run once at boot; the SQLx
  connection pool is shared across all flows and all concurrent requests.
- **`ironcrew run` / `ironcrew inspect` (CLI one-shot)** — each invocation
  creates its own store instance scoped to the flow's `.ironcrew` directory,
  then tears it down when the process exits.

The rest of this document assumes the `serve` singleton model unless stated
otherwise.

## Available Backends

| Backend | Config value | Use case |
|---------|-------------|----------|
| JSON files | `json` (default) | Local development, small deployments, zero config |
| SQLite | `sqlite` | Single-server and Docker deployments, faster queries |
| PostgreSQL | `postgres` | Durable production cloud records and restart recovery. PostgreSQL 15+ required; keep one HTTP executor |

## Configuration

Environment variables control storage:

| Variable | Description | Default |
|----------|-------------|---------|
| `IRONCREW_STORE` | Backend type: `json`, `sqlite`, or `postgres` | `json` |
| `IRONCREW_STORE_PATH` | Custom path for the SQLite database file | `<flow>/.ironcrew/ironcrew.db` |
| `DATABASE_URL` | PostgreSQL 15+ connection string (required when `IRONCREW_STORE=postgres`) | — |
| `IRONCREW_PG_TABLE_PREFIX` | Table name prefix for shared PostgreSQL databases: at most 37 lowercase ASCII alphanumeric/underscore bytes | `""` (table = `runs`) |
| `IRONCREW_DB_POOL_SIZE` | PostgreSQL connection pool size (range 1–128; sized for concurrent HTTP requests, not per-flow) | `10` |
| `IRONCREW_DB_CONNECT_RETRIES` | Connection retries after the initial PostgreSQL connection attempt (range 0–100) | `10` |
| `IRONCREW_DB_CONNECT_BACKOFF_MS` | Base delay for exponential PostgreSQL connection-retry backoff, in milliseconds (range 1–30000) | `1000` |
| `IRONCREW_DB_CONNECT_TIMEOUT_SECS` | PostgreSQL connect/acquire timeout (range 1–120 seconds) | `30` |
| `IRONCREW_INSTANCE_ID` | Optional 1–255 byte printable ASCII process/pod owner identity; generated once per process when absent | generated |
| `IRONCREW_RUN_LEASE_TTL_SECONDS` | Stale-run lease threshold (range 1–86400 seconds) | `60` |
| `IRONCREW_REQUIRE_IDEMPOTENCY_KEY` | Require `Idempotency-Key` on run/message mutation endpoints | `false` |
| `IRONCREW_IDEMPOTENCY_TTL_SECONDS` | Terminal ledger retention, 60–2592000 seconds; must be at least max run lifetime + 3600 | `86400` |
| `IRONCREW_IDEMPOTENCY_MAX_RECORDS` | Maximum retained in-flight and terminal ledger records (hard ceiling 100000) | `10000` |
| `IRONCREW_IDEMPOTENCY_MAX_RECORDS_PER_PRINCIPAL` | Maximum records charged to one authenticated principal; cannot exceed the global cap | global record cap |
| `IRONCREW_IDEMPOTENCY_MAX_IN_FLIGHT_PER_PRINCIPAL` | Maximum concurrent claimed/in-progress records charged to one principal; cannot exceed its record cap | `min(global record cap, 64)` |
| `IRONCREW_IDEMPOTENCY_PRUNE_BATCH` | Maximum expired terminal records removed per claim/startup prune (hard ceiling 10000) | `1000` |
| `IRONCREW_IDEMPOTENCY_MAX_RESPONSE_BYTES` | Maximum compact response stored for one key (hard ceiling 64 MiB) | `8388608` (8 MiB) |
| `IRONCREW_IDEMPOTENCY_MAX_TOTAL_RESPONSE_BYTES` | Aggregate stored response-body budget; excess completions retain a non-replayable tombstone (hard ceiling 8 GiB) | `268435456` (256 MiB) |
| `IRONCREW_IDEMPOTENCY_MAX_TOTAL_RESPONSE_BYTES_PER_PRINCIPAL` | Aggregate stored response-body budget charged to one principal; cannot exceed the global cap | global response-byte cap |
| `IRONCREW_JSON_STORE_RECORD_MAX_BYTES` | Maximum JSON run-record bytes; larger configured values clamp to 128 MiB | `67108864` (64 MiB) |
| `IRONCREW_JSON_STORE_MAX_SCAN_ENTRIES` | Maximum run files visited by one JSON list/count/clean scan; use PostgreSQL above this scale | `10000` (hard ceiling `100000`) |
| `IRONCREW_RUNS_DEFAULT_LIMIT` | Default page size for `GET /flows/{flow}/runs` | `20` |
| `IRONCREW_RUNS_MAX_LIMIT` | Hard cap on `?limit=` for run listing | `100` |
| `IRONCREW_CONVERSATIONS_DEFAULT_LIMIT` | Default page size for `GET /flows/{flow}/conversations` | `20` |
| `IRONCREW_CONVERSATIONS_MAX_LIMIT` | Hard cap on `?limit=` for conversation listing | `100` |

**Note:** The `.ironcrew/` directory is created with `0o700` permissions on Unix
to prevent other users from reading run history.

Set them in your `.env` file, shell environment, or Docker config:

```bash
# .env
IRONCREW_STORE=sqlite

# Or inline
IRONCREW_STORE=sqlite ironcrew run .

# Docker
docker run -e IRONCREW_STORE=sqlite ...
```

## Idempotency ledger

The HTTP run and conversation-message endpoints persist a separate ledger from
run history and conversation rows. Only SHA-256 key digests, an opaque
server-derived principal digest, versioned request fingerprints, fenced
attempt/owner ids, bounded response JSON, leases, and retention timestamps are
stored. Bearer tokens and raw principal labels are never ledger fields.
Deleting a run does not delete its ledger entry, because doing so would make
the same client key executable again during the retention window.

Claims and terminal rows count toward `IRONCREW_IDEMPOTENCY_MAX_RECORDS` and
the authenticated principal's
`IRONCREW_IDEMPOTENCY_MAX_RECORDS_PER_PRINCIPAL`. In-flight work also consumes
`IRONCREW_IDEMPOTENCY_MAX_IN_FLIGHT_PER_PRINCIPAL`, preventing one caller from
occupying every executor slot while retained terminal rows remain available to
other callers.
Expired terminal rows are pruned in bounded batches at startup and while
claiming new operations. In-flight rows are never pruned into a reusable key:
expired message claims become `indeterminate`, while expired run claims are
reconciled to an `abandoned` run with the preallocated run id. When the
aggregate response budget is exhausted, the operation and its fingerprint are
still retained, but replay returns `409` because no response body was stored.
Run acceptance bodies are charged atomically when their claim is inserted, so
later run reconciliation cannot bypass the aggregate cap. Conversation reply
bodies are charged by the atomic transcript-and-ledger commit.
An indeterminate message also retains its exclusive conversation scope. After
inspecting durable history, a client can atomically consume that hazard by
sending a new `Idempotency-Key` plus `Idempotency-Recovery-Key` containing the
prior key; a mismatched or missing recovery key cannot bypass it. The matching
replacement is still held for one full `IRONCREW_RUN_LEASE_TTL_SECONDS` after
the hazard is recorded, so a stale worker cannot overlap the recovered turn.

The per-response cap limits a transient serialization allocation in the HTTP
process. The aggregate cap limits disk/database payload, not permanently
resident Rust heap. For a 1 GiB Railway/OpenShift instance, a conservative
starting policy is 4 MiB per response and 64 MiB aggregate:

```bash
IRONCREW_REQUIRE_IDEMPOTENCY_KEY=true
IRONCREW_IDEMPOTENCY_TTL_SECONDS=86400
IRONCREW_IDEMPOTENCY_MAX_RECORDS=10000
IRONCREW_IDEMPOTENCY_MAX_RECORDS_PER_PRINCIPAL=2500
IRONCREW_IDEMPOTENCY_MAX_IN_FLIGHT_PER_PRINCIPAL=16
IRONCREW_IDEMPOTENCY_MAX_RESPONSE_BYTES=4194304
IRONCREW_IDEMPOTENCY_MAX_TOTAL_RESPONSE_BYTES=67108864
IRONCREW_IDEMPOTENCY_MAX_TOTAL_RESPONSE_BYTES_PER_PRINCIPAL=16777216
```

Size `MAX_RECORDS` from admitted mutation throughput, not only RAM. With a
24-hour TTL, 10,000 rows permit roughly 0.116 new keyed mutations/second before
the ledger is full; bursts can exhaust it sooner. Both global and per-principal
record/response budgets are enforced atomically. Capacity denial returns `429`
with `Retry-After`; response-byte exhaustion instead retains a non-replayable
tombstone so a completed mutation is never executed twice. Protected metrics
publish aggregate and high-water counts only, never principal digests or raw
idempotency keys.

Backend guarantees differ:

- PostgreSQL claims and conversation transcript+ledger commits are database
  transactions and coordinate independent processes. Transaction-scoped
  advisory locks are partitioned by quota, opaque principal, resource,
  exclusive scope, and key, so heartbeats and lookups for unrelated keys do
  not take a table-wide lock. A trigger incrementally maintains compact global
  and per-principal record, in-flight, and response-byte counters; ordinary
  requests do not scan the ledger with `COUNT` or `SUM`. Bootstrap backfills
  pre-principal rows to the legacy principal and reconciles counters during
  bootstrap while ledger DDL excludes concurrent writes.
- SQLite uses `BEGIN IMMEDIATE` transactions and coordinates processes sharing
  the same database file, but it remains a single-host deployment choice.
- JSON stores one owner-only file per key and uses atomic file replacement plus
  a process-local critical section. It is intentionally single-process and
  must not be mounted read/write by multiple pods.

Even with PostgreSQL, live Lua VMs, SSE streams, cancellation, and human-input
bridges remain process-local, so the supported HTTP topology is still one
executor replica.

## JSON File Backend (default)

Run records are stored as individual `.json` files in `<flow>/.ironcrew/runs/`:

```
my-flow/.ironcrew/runs/
├── 3c559b14-aeaa-440c-96ec-0010d2f0c969.json
├── a4d0368b-3f85-4f58-95f8-090999ad510b.json
└── 736380e2-c59a-4d47-be16-c9d99d955030.json
```

**Advantages:**
- Zero configuration — works out of the box
- Human-readable — inspect records with any text editor or `jq`
- No dependencies — no database to install or manage
- Easy backup — just copy the directory

**Limitations:**
- Listing runs requires reading every file (slow with thousands of runs)
- No indexing — status filtering scans all records
- Process-local handles serialize updates to the same runs directory, but JSON
  remains a single-process backend and is not safe on a shared multi-pod volume
- Idempotency records are durable across restarts of that one process, but two
  processes can race file claims; use PostgreSQL for Railway/OpenShift

## SQLite Backend

Run records are stored in a single SQLite database at `<flow>/.ironcrew/ironcrew.db`:

```
my-flow/.ironcrew/
└── ironcrew.db
```

Enable it:

```bash
IRONCREW_STORE=sqlite
```

**Advantages:**
- Fast queries — indexed by `run_id`, sorted by `started_at`
- Status filtering done in SQL, not by scanning files
- Single file — easy to backup, move, or inspect
- ACID transactions — no partial writes
- Handles thousands of runs efficiently

**Limitations:**
- Not human-readable (use `ironcrew inspect` or `sqlite3` CLI)
- Single-writer — concurrent writes are serialized via mutex

### Inspecting the database directly

```bash
# List tables
sqlite3 .ironcrew/ironcrew.db ".tables"

# Query runs
sqlite3 .ironcrew/ironcrew.db "SELECT run_id, status, duration_ms FROM runs"

# Count by status
sqlite3 .ironcrew/ironcrew.db "SELECT status, count(*) FROM runs GROUP BY status"

# Export to JSON
sqlite3 .ironcrew/ironcrew.db -json "SELECT * FROM runs ORDER BY started_at DESC LIMIT 5"
```

### Schema

```sql
CREATE TABLE runs (
    run_id        TEXT PRIMARY KEY,
    flow_name     TEXT NOT NULL,      -- crew goal (human-readable)
    flow          TEXT NOT NULL DEFAULT '', -- flow slug the run was launched under (scoping key)
    status        TEXT NOT NULL,
    started_at    TEXT NOT NULL,
    finished_at   TEXT NOT NULL,
    duration_ms   INTEGER NOT NULL,
    task_results  TEXT NOT NULL,    -- JSON array
    agent_count   INTEGER NOT NULL,
    task_count    INTEGER NOT NULL,
    total_tokens  INTEGER DEFAULT 0,
    cached_tokens INTEGER DEFAULT 0,
    tags          TEXT DEFAULT '[]', -- JSON array
    created_at    TEXT DEFAULT (datetime('now'))
);
```

## Custom SQLite Path

Override the default database location:

```bash
# Shared database for all flows
IRONCREW_STORE=sqlite
IRONCREW_STORE_PATH=/data/ironcrew-runs.db

# Per-environment databases
IRONCREW_STORE_PATH=./data/production.db
```

## PostgreSQL Backend

PostgreSQL is included by default in the standard binary. To build a minimal
binary without PostgreSQL support:

```bash
cargo build --release --locked --no-default-features
```

Configure:

```bash
IRONCREW_STORE=postgres
DATABASE_URL=postgres://user:password@localhost:5432/ironcrew
```

**Version requirement:** PostgreSQL 15 or newer is required. IronCrew depends
on PostgreSQL 15 features for flow-scoped session uniqueness and is intended
for extension-capable deployments such as installations that use `pgvector`.

**Advantages:**
- Durable records shared independently of the container filesystem
- **JSONB columns** for `task_results` and `tags` — query into JSON natively with SQL
- Full SQL querying power (joins, aggregation, GIN indexes on JSONB)
- Production-grade durability and replication
- Async I/O — non-blocking database operations via `sqlx`

**Limitations:**
- Requires an external PostgreSQL server
- Requires PostgreSQL 15+
- Adds compile-time dependency on `sqlx`
- Does not distribute active run handles, conversation Lua VMs, pending human
  questions, cancellation, or SSE state; `ironcrew serve` therefore remains a
  one-replica deployment

### Schema

The table is auto-created on first connection. Uses **JSONB** for `task_results`
and `tags`, enabling native PostgreSQL JSON queries:

```sql
CREATE TABLE IF NOT EXISTS runs (
    run_id        TEXT PRIMARY KEY,
    flow_name     TEXT NOT NULL,      -- crew goal (human-readable)
    flow          TEXT NOT NULL DEFAULT '', -- flow slug the run was launched under (scoping key)
    status        TEXT NOT NULL,
    started_at    TEXT NOT NULL,
    finished_at   TEXT NOT NULL,
    duration_ms   BIGINT NOT NULL,
    task_results  JSONB NOT NULL DEFAULT '[]',
    agent_count   INTEGER NOT NULL,
    task_count    INTEGER NOT NULL,
    total_tokens  INTEGER DEFAULT 0,
    cached_tokens INTEGER DEFAULT 0,
    tags          JSONB DEFAULT '[]',
    created_at    TIMESTAMPTZ DEFAULT NOW()
);
```

### Querying JSONB data

Other applications can query run data directly with SQL, without going through
IronCrew's API:

```sql
-- Find runs tagged with "v2-prompt"
SELECT run_id, flow_name, status FROM runs
WHERE tags @> '["v2-prompt"]';

-- Find runs where a specific task failed
SELECT run_id FROM runs
WHERE task_results @> '[{"task":"research","success":false}]';

-- Count tokens per flow
SELECT flow_name, SUM(total_tokens) as total
FROM runs GROUP BY flow_name;

-- Get runs from the last 24 hours
SELECT * FROM runs
WHERE created_at > NOW() - INTERVAL '24 hours'
ORDER BY started_at DESC;

-- Add a GIN index for fast JSONB queries
CREATE INDEX idx_runs_tags ON runs USING GIN (tags);
CREATE INDEX idx_runs_task_results ON runs USING GIN (task_results);
```

### Docker with PostgreSQL

PostgreSQL is enabled in the standard image. Keep the DSN in a secret-bearing
environment file rather than a Docker layer:

```bash
docker build --pull -t ironcrew .
docker run --env-file .env \
  -e IRONCREW_STORE=postgres \
  -v ./flows:/flows:ro \
  -p 3000:3000 \
  ironcrew
```

### Shared Database with Table Prefix

Multiple IronCrew projects can share a single PostgreSQL database using
`IRONCREW_PG_TABLE_PREFIX`:

Prefixes are limited to 37 lowercase ASCII letters, digits, and underscores so
every derived table and index name remains below PostgreSQL's 63-byte identifier
limit without case-folding or truncation collisions.

```bash
# Project A
IRONCREW_PG_TABLE_PREFIX=projecta_ ironcrew serve
# → table: projecta_runs

# Project B
IRONCREW_PG_TABLE_PREFIX=projectb_ ironcrew serve
# → table: projectb_runs

# No prefix (default)
# → table: runs
```

Each prefix gets its own table, fully isolated within the same database.

### Building without PostgreSQL

PostgreSQL is included by default. To build a smaller binary without it:

```bash
cargo build --release --locked --no-default-features
```

If you set `IRONCREW_STORE=postgres` on a binary built without PostgreSQL, you
get a clear error:

```
Validation error: PostgreSQL backend requires building with --features postgres
```

## How Stores Are Used

All IronCrew features use the same store:

| Feature | Store operation |
|---------|----------------|
| `crew:run()` | `save_run_intent` at start, then `update_run_completion` at termination |
| `crew:ask_human()` | `update_run_status` — transitions between `running` and `waiting_for_input` |
| process startup | `reconcile_abandoned_runs` — marks only expired/unleased in-flight runs as `abandoned` |
| non-keyed run heartbeat | `heartbeat_owned_runs` — refreshes unlinked run leases owned by this process |
| keyed HTTP run heartbeat | `heartbeat_idempotent_run` — atomically verifies and refreshes the matching operation ledger and run lease |
| readiness | `health_check` — performs a minimal backend round-trip |
| `ironcrew runs` | `list_runs_summary` + `count_runs` — paginated metadata listing |
| `ironcrew inspect` | `get_run` — retrieves a specific run by ID |
| `ironcrew clean` | `list_runs_summary` + `delete_run` — removes old records |
| `GET /flows/{flow}/runs` | `list_runs_summary` + `count_runs` — paginated API endpoint |
| `GET /flows/{flow}/runs/{id}` | `get_run` — API endpoint |
| `DELETE /flows/{flow}/runs/{id}` | `delete_run` — API endpoint |
| `ironcrew run --json` | `get_run` — reads back the saved record for output |
| `crew:conversation({id=...})` | `save_conversation` / `get_conversation` — resume-by-id chat sessions |
| `crew:dialog({id=...})` | `save_dialog_state` / `get_dialog_state` — resume-by-id multi-agent dialogs |

### Run ownership and terminal writes

An in-flight record carries an owner id and lease deadline. Non-keyed work is
renewed by the process heartbeat. A keyed HTTP run is deliberately excluded
from that broad heartbeat: its monitor atomically renews both the run record
and the matching idempotency attempt, so a detached or fenced worker cannot be
kept alive accidentally by process maintenance. Startup reconciliation
abandons only expired (or legacy unleased) records, never a healthy run owned
by another process. `update_run_completion` is an owner-checked
compare-and-set: the first terminal writer wins, and a later timeout, abort,
panic, or completion cannot replace that terminal payload. This protects
restart recovery, but does not make process-local HTTP control state
horizontally scalable.

## The StateStore Trait

The storage system is built on a single async trait covering run lifecycle and
history, persistent sessions, and the audit log. Run listing is paginated and
metadata-first; sessions use stable IDs, flow scoping, and revision-guarded
updates. Listing uses
`list_runs_summary` + `count_runs` so a caller never pays to transfer
`task_results` when they only need a summary view.

```rust
#[async_trait]
pub trait StateStore: Send + Sync {
    // ─── Run history ────────────────────────────────────────────────
    async fn save_run_intent(&self, intent: RunIntent) -> Result<String>;
    async fn update_run_completion(
        &self,
        run_id: &str,
        completion: RunCompletion,
    ) -> Result<RunTransition>;
    async fn update_run_status(&self, run_id: &str, status: RunStatus) -> Result<()>;
    fn instance_id(&self) -> &str;
    fn run_lease_ttl(&self) -> Duration;
    async fn heartbeat_owned_runs(&self) -> Result<usize>;
    async fn health_check(&self) -> Result<()>;
    async fn reconcile_abandoned_runs(&self, now: &str) -> Result<usize>;
    async fn get_run(&self, run_id: &str) -> Result<RunRecord>;

    /// Paginated, metadata-only list. `limit=0` means unlimited.
    async fn list_runs_summary(
        &self,
        filter: &ListRunsFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunSummary>>;

    async fn count_runs(&self, filter: &ListRunsFilter) -> Result<u64>;
    async fn delete_run(&self, run_id: &str) -> Result<()>;

    // ─── Durable HTTP idempotency ──────────────────────────────────
    async fn lookup_idempotency(
        &self,
        key_hash: &str,
        request_fingerprint: &str,
        now: &str,
    ) -> Result<IdempotencyLookup>;
    async fn claim_idempotency(
        &self,
        claim: IdempotencyClaim,
        max_records: usize,
        prune_batch: usize,
    ) -> Result<IdempotencyClaimOutcome>;
    async fn heartbeat_idempotency(
        &self,
        key_hash: &str,
        attempt_id: &str,
        new_lease_expires_at: &str,
    ) -> Result<bool>;
    async fn heartbeat_idempotent_run(
        &self,
        run_id: &str,
        key_hash: &str,
        attempt_id: &str,
        new_lease_expires_at: &str,
    ) -> Result<RunFenceHeartbeat>;
    async fn complete_idempotency(
        &self,
        completion: IdempotencyCompletion,
        max_total_response_bytes: usize,
    ) -> Result<IdempotencyCompletionOutcome>;
    async fn commit_conversation_idempotency(
        &self,
        completion: IdempotencyCompletion,
        conversation: &ConversationRecord,
        max_total_response_bytes: usize,
    ) -> Result<ConversationIdempotencyCommit>;
    async fn mark_idempotency_indeterminate(
        &self,
        key_hash: &str,
        attempt_id: &str,
        completed_at: &str,
        expires_at: &str,
    ) -> Result<bool>;
    async fn release_idempotency(&self, key_hash: &str, attempt_id: &str) -> Result<bool>;
    async fn prune_idempotency(&self, now: &str, limit: usize) -> Result<usize>;

    // ─── Persistent sessions ────────────────────────────────────────
    async fn save_conversation(&self, record: &ConversationRecord) -> Result<u64>;
    async fn get_conversation(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<Option<ConversationRecord>>;
    async fn delete_conversation(&self, flow_path: Option<&str>, id: &str) -> Result<()>;

    async fn list_conversations(
        &self,
        flow_path: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<ConversationSummary>>;
    async fn count_conversations(&self, flow_path: Option<&str>) -> Result<u64>;

    async fn save_dialog_state(&self, record: &DialogStateRecord) -> Result<()>;
    async fn get_dialog_state(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<Option<DialogStateRecord>>;
    async fn delete_dialog_state(&self, flow_path: Option<&str>, id: &str) -> Result<()>;

    // ─── Audit log ─────────────────────────────────────────
    async fn save_audit_event(&self, event: &AuditEvent) -> Result<String>;
    async fn list_audit_events(
        &self,
        filter: &AuditFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<AuditEvent>>;
    async fn count_audit_events(&self, filter: &AuditFilter) -> Result<u64>;
}
```

### Conversation scoping (`flow_path`)

Conversations and dialogs are keyed by the composite `(flow_path, id)` pair,
not by `id` alone. This means two different flows can use the same session
id (`"alice-support"`) without colliding.

- `flow_path = Some(slug)` passed to `get_conversation` /
  `delete_conversation` / `list_conversations` / `count_conversations`
  means **"only records belonging to this flow"**. Legacy records written
  before the column existed have `flow_path = NULL` and are **invisible**
  to scoped queries.
- `flow_path = None` passed to `get_conversation` / `delete_conversation`
  is an **admin / global lookup** — it matches any record with the given
  `id` regardless of which flow (if any) owns it. The `ironcrew inspect`
  CLI uses this form.
- **JSON backend:** records live at
  `<conversations_dir>/<flow>/<id>.json` (flow-namespaced subdirectories).
  Legacy flat paths `<conversations_dir>/<id>.json` are still readable as
  a fallback for in-place upgrades.
- **SQL backends:** the `{prefix}conversations` and `{prefix}dialogs`
  tables have a `flow_path TEXT` column added via idempotent
  `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` (Postgres) / `ALTER TABLE
  ... ADD COLUMN flow_path TEXT` guarded against "duplicate column"
  errors (SQLite). Indexes `idx_{prefix}conversations_flow_path` and
  `idx_{prefix}dialogs_flow_path` back flow-scoped listing queries.

`ListRunsFilter` has three optional fields: `status`, `tag`, and `since`
(RFC3339 timestamp). All three are composed with `AND` when multiple are
set. `RunSummary` is `RunRecord` minus `task_results` — the field that
typically dominates a record's on-disk size.

**Sessions vs runs:** `get_*` returns `Option` for sessions (so the caller
can distinguish "first time this id is used" from a real error) but `Result`
for runs (because `get_run` is always called with an id the caller believes
exists). Session saves use optimistic concurrency: each loaded snapshot carries
a `revision`, a successful save returns the next revision, and a stale writer
fails with a conflict instead of overwriting turns completed by another pod.

### Session storage layout

| Backend     | Conversations                                            | Dialogs                                             |
|-------------|-----------------------------------------------------------|------------------------------------------------------|
| `json`      | `.ironcrew/conversations/<flow>/<id>.json`                | `.ironcrew/dialogs/<flow>/<id>.json`                 |
| `sqlite`    | `conversations` table in `.ironcrew/ironcrew.db`          | `dialogs` table in the same file                     |
| `postgres`  | `{prefix}conversations` table                             | `{prefix}dialogs` table                              |

Flow-namespaced subdirectories in the JSON backend keep sessions isolated
per flow; a legacy flat `<id>.json` layout from earlier versions is still
readable as a fallback. All JSON subdirectories are created at `0o700` on
Unix. SQLite and PostgreSQL tables are created on first connect via
`CREATE TABLE IF NOT EXISTS`. PostgreSQL also creates B-tree indexes on
`flow_path` (`idx_{prefix}conversations_flow_path`,
`idx_{prefix}dialogs_flow_path`) and `updated_at` to back flow-scoped
listing queries.

### Session table schema (PostgreSQL)

```sql
CREATE TABLE IF NOT EXISTS {prefix}conversations (
    id          TEXT NOT NULL,
    flow_name   TEXT NOT NULL,
    flow_path   TEXT,
    agent_name  TEXT NOT NULL,
    messages    JSONB NOT NULL DEFAULT '[]',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    revision    BIGINT NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX uniq_{prefix}conversations_flow_id
    ON {prefix}conversations (flow_path, id) NULLS NOT DISTINCT;

CREATE INDEX IF NOT EXISTS idx_{prefix}conversations_updated_at
    ON {prefix}conversations (updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_{prefix}conversations_flow_name
    ON {prefix}conversations (flow_name);
CREATE INDEX IF NOT EXISTS idx_{prefix}conversations_flow_path
    ON {prefix}conversations (flow_path);

CREATE TABLE IF NOT EXISTS {prefix}dialogs (
    id          TEXT NOT NULL,
    flow_name   TEXT NOT NULL,
    flow_path   TEXT,
    agent_names JSONB NOT NULL DEFAULT '[]',
    starter     TEXT NOT NULL,
    transcript  JSONB NOT NULL DEFAULT '[]',
    next_index  INTEGER NOT NULL,
    stopped     BOOLEAN NOT NULL DEFAULT FALSE,
    stop_reason TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    revision    BIGINT NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX uniq_{prefix}dialogs_flow_id
    ON {prefix}dialogs (flow_path, id) NULLS NOT DISTINCT;

CREATE INDEX IF NOT EXISTS idx_{prefix}dialogs_updated_at
    ON {prefix}dialogs (updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_{prefix}dialogs_flow_name
    ON {prefix}dialogs (flow_name);
CREATE INDEX IF NOT EXISTS idx_{prefix}dialogs_flow_path
    ON {prefix}dialogs (flow_path);
```

### Session table schema (SQLite)

```sql
CREATE TABLE IF NOT EXISTS conversations (
    id         TEXT NOT NULL,
    flow_name  TEXT NOT NULL,
    flow_path  TEXT,
    agent_name TEXT NOT NULL,
    messages   TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    revision   INTEGER NOT NULL DEFAULT 0,
    UNIQUE (flow_path, id)
);

CREATE TABLE IF NOT EXISTS dialogs (
    id          TEXT NOT NULL,
    flow_name   TEXT NOT NULL,
    flow_path   TEXT,
    agent_names TEXT NOT NULL,
    starter     TEXT NOT NULL,
    transcript  TEXT NOT NULL,
    next_index  INTEGER NOT NULL,
    stopped     INTEGER NOT NULL DEFAULT 0,
    stop_reason TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    revision    INTEGER NOT NULL DEFAULT 0,
    UNIQUE (flow_path, id)
);
```

Bootstrap migrates legacy `id PRIMARY KEY` session tables to these composite
keys and adds `revision` with a zero default. Revision zero is accepted only
for a new row or the first guarded update of a legacy row.

### Session ID validation

User-supplied session IDs are restricted to ASCII alphanumerics plus `-`,
`_`, and `.`, and must be 1-128 characters. The restriction runs at the
Lua layer (`src/engine/sessions.rs::validate_session_id`) before the id
ever reaches a backend, which prevents:

- Path traversal against the JSON store (e.g. `../etc/passwd`).
- SQL metacharacter oddness against SQLite/PostgreSQL.
- Silent truncation on filesystems with short filename limits.

Violations surface as a clear `Validation` error.

This design allows future backends (Redis, cloud storage) to use async I/O
natively without blocking the Tokio runtime.

## Switching Backends

Changing `IRONCREW_STORE` does **not** migrate existing data. If you switch from
`json` to `sqlite`, previously stored JSON runs remain in the `runs/` directory
but will not appear in queries against the SQLite store.

To migrate, read records from the old store and insert into the new one:

```bash
# Example: read JSON runs and re-save to SQLite
for f in .ironcrew/runs/*.json; do
    ironcrew inspect $(basename "$f" .json) -p .  # verify it reads
done
# Then switch to sqlite and re-run your flows
```

A future `ironcrew migrate` command may automate this.

## Choosing a Backend

| Scenario | Recommended |
|----------|-------------|
| Local development | `json` (default) — zero setup |
| Docker deployment (single instance) | `sqlite` — single file, fast queries |
| Many runs (100+) | `sqlite` — indexed, no file scanning |
| Debugging runs | `json` — human-readable files |
| CI/CD pipelines | `json` — ephemeral, no state needed |
| Production single-server | `sqlite` — handles concurrent reads well |
| Production HTTP service | `postgres` — durable shared records, with exactly one live HTTP executor |
| Cloud deployment (Railway, OpenShift) | `postgres` — managed/cluster database available; keep the service at one replica |
