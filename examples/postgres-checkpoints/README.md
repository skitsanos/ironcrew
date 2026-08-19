# PostgreSQL Checkpoints

Demonstrates the `postgres.*` Lua namespace: a flow-defined, project-declared
SQL operation used to checkpoint intermediate task output to a dedicated app
database, then read it back. See [docs/postgres-app-data.md](../../docs/postgres-app-data.md)
for the full trust model, operation format, and limits.

## What it does

1. Runs a one-task crew that asks an agent to list three benefits of Rust for
   systems programming.
2. Checkpoints the task's output via `postgres.execute("save_checkpoint", ...)`
   — an upsert keyed on `execution_id .. ':' .. stage`.
3. Reads the checkpoint back via `postgres.query("load_checkpoints", ...)` and
   prints how many rows were stored.

## Required table

`postgres.*` does not create or migrate schema — the operator is responsible
for the schema existing before a flow runs. Create it once against the
database named by `IRONCREW_APP_DATABASE_URL`:

```sql
CREATE TABLE checkpoints (
    idempotency_key text PRIMARY KEY,
    execution_id text NOT NULL,
    stage text NOT NULL,
    payload jsonb NOT NULL
);
```

## Run it

```bash
cp examples/postgres-checkpoints/.env.example examples/postgres-checkpoints/.env
# Fill in OPENAI_API_KEY and IRONCREW_APP_DATABASE_URL in the copied file.

ironcrew validate examples/postgres-checkpoints
ironcrew run examples/postgres-checkpoints
```

Without `IRONCREW_APP_DATABASE_URL` set, both `postgres.*` calls fail with a
configuration hint rather than a silent no-op — validate that the variable is
set before assuming the checkpoint step is broken.

## At-least-once note

Task retries and `foreach` re-runs are core engine behavior, so `postgres.*`
does not promise exactly-once execution. `save_checkpoint` is written as an
`ON CONFLICT` upsert keyed on `idempotency_key` (`execution_id || ':' ||
stage` in SQL, `execution_id .. ":" .. stage` in Lua), so re-running the
`analyze` stage for the same `execution_id` overwrites the same row instead
of accumulating duplicates. Design every write operation this way unless the
table's own constraints already make retries safe.

## Files

| File | Purpose |
|---|---|
| `crew.lua` | The flow: one agent, one task, then a checkpoint save + read |
| `sql/save_checkpoint.sql` | Named `execute` operation — upsert on `idempotency_key` |
| `sql/load_checkpoints.sql` | Named `query` operation — reads back rows for one `execution_id` |
| `.env.example` | Copy to `.env`; provider key plus the app-data URL |
