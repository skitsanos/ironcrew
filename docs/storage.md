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
| PostgreSQL | `postgres` | Production cloud, multi-instance, shared state. PostgreSQL 15+ required |

## Configuration

Environment variables control storage:

| Variable | Description | Default |
|----------|-------------|---------|
| `IRONCREW_STORE` | Backend type: `json`, `sqlite`, or `postgres` | `json` |
| `IRONCREW_STORE_PATH` | Custom path for the SQLite database file | `<flow>/.ironcrew/ironcrew.db` |
| `DATABASE_URL` | PostgreSQL 15+ connection string (required when `IRONCREW_STORE=postgres`) | — |
| `IRONCREW_PG_TABLE_PREFIX` | Table name prefix for shared PostgreSQL databases. Only alphanumeric and underscore allowed (`^[a-zA-Z0-9_]*$`) | `""` (table = `runs`) |
| `IRONCREW_DB_POOL_SIZE` | PostgreSQL connection pool size (sized for concurrent HTTP requests, not per-flow) | `10` |
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
- Concurrent writes may conflict (rare in practice)

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
    flow_name     TEXT NOT NULL,
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
cargo build --release --no-default-features
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
- Shared state across multiple IronCrew instances
- **JSONB columns** for `task_results` and `tags` — query into JSON natively with SQL
- Full SQL querying power (joins, aggregation, GIN indexes on JSONB)
- Production-grade durability and replication
- Async I/O — non-blocking database operations via `sqlx`

**Limitations:**
- Requires an external PostgreSQL server
- Requires PostgreSQL 15+
- Adds compile-time dependency on `sqlx`

### Schema

The table is auto-created on first connection. Uses **JSONB** for `task_results`
and `tags`, enabling native PostgreSQL JSON queries:

```sql
CREATE TABLE IF NOT EXISTS runs (
    run_id        TEXT PRIMARY KEY,
    flow_name     TEXT NOT NULL,
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

```dockerfile
# Build with postgres support
FROM rust:latest AS builder
RUN cargo build --release --features postgres

# Runtime
FROM debian:bookworm-slim
COPY --from=builder /app/target/release/ironcrew /usr/local/bin/
ENV IRONCREW_STORE=postgres
ENV DATABASE_URL=postgres://user:pass@db:5432/ironcrew
CMD ["ironcrew", "serve", "--host", "0.0.0.0"]
```

### Shared Database with Table Prefix

Multiple IronCrew projects can share a single PostgreSQL database using
`IRONCREW_PG_TABLE_PREFIX`:

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
cargo build --release --no-default-features
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
| `crew:run()` | `save_run` — saves the run record after execution |
| `ironcrew runs` | `list_runs_summary` + `count_runs` — paginated metadata listing |
| `ironcrew inspect` | `get_run` — retrieves a specific run by ID |
| `ironcrew clean` | `list_runs_summary` + `delete_run` — removes old records |
| `GET /flows/{flow}/runs` | `list_runs_summary` + `count_runs` — paginated API endpoint |
| `GET /flows/{flow}/runs/{id}` | `get_run` — API endpoint |
| `DELETE /flows/{flow}/runs/{id}` | `delete_run` — API endpoint |
| `ironcrew run --json` | `get_run` — reads back the saved record for output |
| `crew:conversation({id=...})` | `save_conversation` / `get_conversation` — resume-by-id chat sessions |
| `crew:dialog({id=...})` | `save_dialog_state` / `get_dialog_state` — resume-by-id multi-agent dialogs |

## The StateStore Trait

The storage system is built on a single async trait covering both run
history (paginated, metadata-first) and persistent sessions (stable-id,
upsert-style). Listing uses `list_runs_summary` + `count_runs` so a
caller never pays to transfer `task_results` when they only need a
summary view.

```rust
#[async_trait]
pub trait StateStore: Send + Sync {
    // ─── Run history ────────────────────────────────────────────────
    async fn save_run(&self, record: &RunRecord) -> Result<String>;
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

    // ─── Persistent sessions ────────────────────────────────────────
    async fn save_conversation(&self, record: &ConversationRecord) -> Result<()>;
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
    async fn get_dialog_state(&self, id: &str) -> Result<Option<DialogStateRecord>>;
    async fn delete_dialog_state(&self, id: &str) -> Result<()>;
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
exists). Session `save_*` is idempotent upsert — calling it with an
existing id overwrites the prior record.

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
    id          TEXT PRIMARY KEY,
    flow_name   TEXT NOT NULL,
    agent_name  TEXT NOT NULL,
    messages    JSONB NOT NULL DEFAULT '[]',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
ALTER TABLE {prefix}conversations ADD COLUMN IF NOT EXISTS flow_path TEXT;

CREATE INDEX IF NOT EXISTS idx_{prefix}conversations_updated_at
    ON {prefix}conversations (updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_{prefix}conversations_flow_name
    ON {prefix}conversations (flow_name);
CREATE INDEX IF NOT EXISTS idx_{prefix}conversations_flow_path
    ON {prefix}conversations (flow_path);

CREATE TABLE IF NOT EXISTS {prefix}dialogs (
    id          TEXT PRIMARY KEY,
    flow_name   TEXT NOT NULL,
    agent_names JSONB NOT NULL DEFAULT '[]',
    starter     TEXT NOT NULL,
    transcript  JSONB NOT NULL DEFAULT '[]',
    next_index  INTEGER NOT NULL,
    stopped     BOOLEAN NOT NULL DEFAULT FALSE,
    stop_reason TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
ALTER TABLE {prefix}dialogs ADD COLUMN IF NOT EXISTS flow_path TEXT;

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
    id         TEXT PRIMARY KEY,
    flow_name  TEXT NOT NULL,
    agent_name TEXT NOT NULL,
    messages   TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
-- Idempotent migration for pre-flow_path schemas (duplicate-column errors
-- are swallowed):
ALTER TABLE conversations ADD COLUMN flow_path TEXT;

CREATE TABLE IF NOT EXISTS dialogs (
    id          TEXT PRIMARY KEY,
    flow_name   TEXT NOT NULL,
    agent_names TEXT NOT NULL,
    starter     TEXT NOT NULL,
    transcript  TEXT NOT NULL,
    next_index  INTEGER NOT NULL,
    stopped     INTEGER NOT NULL DEFAULT 0,
    stop_reason TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
ALTER TABLE dialogs ADD COLUMN flow_path TEXT;
```

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
| Production multi-instance | `postgres` — shared state, replication |
| Cloud deployment (Railway, Fly.io) | `postgres` — managed database available |
