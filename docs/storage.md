# Storage Backends

IronCrew uses a pluggable storage system for persisting run records, powered by
the `StateStore` trait. Each flow gets its own store instance based on its
`.ironcrew` directory, keeping data isolated between flows.

## Available Backends

| Backend | Config value | Use case |
|---------|-------------|----------|
| JSON files | `json` (default) | Local development, small deployments, zero config |
| SQLite | `sqlite` | Single-server and Docker deployments, faster queries |
| PostgreSQL | `postgres` | Production cloud, multi-instance, shared state |

## Configuration

Environment variables control storage:

| Variable | Description | Default |
|----------|-------------|---------|
| `IRONCREW_STORE` | Backend type: `json`, `sqlite`, or `postgres` | `json` |
| `IRONCREW_STORE_PATH` | Custom path for the SQLite database file | `<flow>/.ironcrew/ironcrew.db` |
| `DATABASE_URL` | PostgreSQL connection string (required when `IRONCREW_STORE=postgres`) | — |
| `IRONCREW_PG_TABLE_PREFIX` | Table name prefix for shared PostgreSQL databases | `""` (table = `runs`) |

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

PostgreSQL support is behind a feature flag to keep the default binary lean.
Build with the `postgres` feature:

```bash
cargo build --release --features postgres
```

Then configure:

```bash
IRONCREW_STORE=postgres
DATABASE_URL=postgres://user:password@localhost:5432/ironcrew
```

**Advantages:**
- Shared state across multiple IronCrew instances
- Full SQL querying power (joins, aggregation, full-text search)
- Production-grade durability and replication
- Async I/O — non-blocking database operations via `sqlx`

**Limitations:**
- Requires an external PostgreSQL server
- Adds compile-time dependency on `sqlx`
- Build with `--features postgres` required

### Schema

The table is auto-created on first connection:

```sql
CREATE TABLE IF NOT EXISTS runs (
    run_id        TEXT PRIMARY KEY,
    flow_name     TEXT NOT NULL,
    status        TEXT NOT NULL,
    started_at    TEXT NOT NULL,
    finished_at   TEXT NOT NULL,
    duration_ms   BIGINT NOT NULL,
    task_results  TEXT NOT NULL,
    agent_count   INTEGER NOT NULL,
    task_count    INTEGER NOT NULL,
    total_tokens  INTEGER DEFAULT 0,
    cached_tokens INTEGER DEFAULT 0,
    tags          TEXT DEFAULT '[]',
    created_at    TIMESTAMPTZ DEFAULT NOW()
);
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

### Without the feature flag

PostgreSQL is included by default. To build without it (smaller binary):

```bash
cargo build --release --no-default-features
```

If you set `IRONCREW_STORE=postgres` on a binary built without the postgres feature,
you get a clear error:

```
Validation error: PostgreSQL backend requires building with --features postgres
```

## How Stores Are Used

All IronCrew features use the same store:

| Feature | Store operation |
|---------|----------------|
| `crew:run()` | `save_run` — saves the run record after execution |
| `ironcrew runs` | `list_runs` — lists records, optional status filter |
| `ironcrew inspect` | `get_run` — retrieves a specific run by ID |
| `ironcrew clean` | `list_runs` + `delete_run` — removes old records |
| `GET /flows/{flow}/runs` | `list_runs` — API endpoint |
| `GET /flows/{flow}/runs/{id}` | `get_run` — API endpoint |
| `DELETE /flows/{flow}/runs/{id}` | `delete_run` — API endpoint |
| `ironcrew run --json` | `get_run` — reads back the saved record for output |

## The StateStore Trait

The storage system is built on an async trait:

```rust
#[async_trait]
pub trait StateStore: Send + Sync {
    async fn save_run(&self, record: &RunRecord) -> Result<String>;
    async fn get_run(&self, run_id: &str) -> Result<RunRecord>;
    async fn list_runs(&self, status_filter: Option<&str>) -> Result<Vec<RunRecord>>;
    async fn delete_run(&self, run_id: &str) -> Result<()>;
}
```

This design allows future backends (PostgreSQL, Redis, cloud storage) to use
async I/O natively without blocking the Tokio runtime.

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
