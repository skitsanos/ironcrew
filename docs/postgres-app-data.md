# PostgreSQL App Data (`postgres.*`)

Crew scripts often need to store intermediate and final execution results in
their own PostgreSQL schema — the role an application database plays
alongside IronCrew's own run history. The `postgres.*` Lua namespace gives
`crew.lua` (and `config.lua`) a way to run **named, project-declared SQL
operations** against a dedicated database, completely separate from the
`StateStore` that persists run/conversation records (see
[storage.md](storage.md)).

This is not a general SQL escape hatch. There is no `postgres.execute_sql(...)`
that takes a raw query string. Every call names one operation declared as a
`.sql` file in the project; the runtime binds parameters positionally and
never interpolates flow-supplied values into SQL text.

## Trust model

Three actors, three boundaries. Named operations are **not** the boundary
against flow authors — the database role is. This is stated plainly because
it is easy to assume operation names imply per-operation containment; they
don't.

| Actor | Boundary |
|---|---|
| Operator | Grants the capability by setting `IRONCREW_APP_DATABASE_URL` — a URL separate from `DATABASE_URL` (the internal `StateStore`'s PostgreSQL connection string). No URL → every `postgres.*` call fails with a clear "capability not configured" error. The URL is not readable from Lua by default; like every environment value, it becomes readable only if the operator explicitly lists it in `IRONCREW_ENV_ALLOWLIST` — never allowlist `IRONCREW_APP_DATABASE_URL` (or `DATABASE_URL`). The recommended deployment uses a dedicated role with minimal `GRANT`s on a dedicated schema. |
| Flow author | Declares operations as SQL files in the project (`sql/*.sql`). They control the SQL, so their ceiling is whatever the database role permits. Named operations give reviewability and fingerprintability, not author containment. |
| Agent | No access in v1. The `postgres` namespace registers only in the crew sandbox (`crew.lua` / `config.lua`), never in the tool VM used by `tools/*.lua`. A follow-up may add an agent-facing tool wrapper gated on an explicit per-operation allowlist plus `require_approval` support. |

If you need to keep flow authors from doing something the database role would
otherwise allow, restrict the role's `GRANT`s — not the operation names.

## Declaring operations: the `sql/` directory

One file per operation under `<project>/sql/`. The operation name is the file
stem, validated with the same rule used for provider names elsewhere in
IronCrew: ASCII letters, digits, `_`, `-`; non-empty; bounded bytes.

```sql
-- ironcrew:op
-- params: execution_id text, stage text, payload json
INSERT INTO checkpoints (idempotency_key, execution_id, stage, payload)
VALUES ($1 || ':' || $2, $1, $2, $3)
ON CONFLICT (idempotency_key) DO UPDATE SET payload = EXCLUDED.payload;
```

Rules:

- The first line must be exactly `-- ironcrew:op`. An optional
  `-- params: name type, name type, ...` header line declares parameters in
  order. Lua passes parameters **by name** in a table; the runtime binds them
  **positionally** in declared order (`$1`, `$2`, ...). A params table with a
  missing or an unknown parameter name is a load-time/call-time error — same
  unknown-key-rejection philosophy as IC-028.
- Parameter types are a closed set: `text`, `integer`, `double`, `boolean`,
  `json`.
- **JSON parameters bind natively as JSONB.** `json`-typed values are bound
  directly as `serde_json::Value` through sqlx's Postgres `json` feature —
  there is no `::jsonb` cast to write in the declared SQL, and a `json`/
  `jsonb` result column decodes back into a Lua table with no extra parsing
  step. (Earlier design notes described `json` params as "serialized to text,
  cast with `::jsonb`"; that text-plus-cast path was superseded during
  implementation and is not what the shipped runtime does.)
- Values are always bound parameters. There is no interpolation path
  anywhere in the design — a flow cannot build SQL text from user input and
  hand it to `postgres.*`.
- `query` and `query_one` operations must contain **exactly one** SQL
  statement.
- `execute` operations may contain multiple `;`-separated statements. All of
  them run inside **one transaction**, managed entirely in Rust
  (`BEGIN … COMMIT`). No transaction handle ever crosses into Lua — there is
  no `begin()`/`commit()` API — so a cancelled task or a Lua yield cannot
  strand an open transaction; dropping the connection rolls the work back
  server-side.
- Operations are discovered alongside `agents/` and `tools/`, subject to
  `IRONCREW_APP_DB_MAX_OPERATIONS` (operation count) and
  `IRONCREW_APP_DB_MAX_SQL_BYTES` (bytes per file). In HTTP conversation mode,
  operation sources come from the conversation's immutable snapshot, like
  every other project file.

## Lua API (crew sandbox only)

```lua
local n    = postgres.execute("save_checkpoint", {
    execution_id = execution_id,
    stage = "analyze",
    payload = { output = results[1].output },
})
local rows = postgres.query("load_checkpoints", { execution_id = execution_id })
local row  = postgres.query_one("latest_stage", { execution_id = execution_id })
```

| Call | Returns |
|---|---|
| `postgres.execute(name, params?)` | Total rows affected across all statements in the operation (integer). |
| `postgres.query(name, params?)` | An array of row tables. |
| `postgres.query_one(name, params?)` | One row table, or `nil` for zero rows. **Errors if more than one row matches** — a deterministic contract; silently taking the first row would hide a bad predicate. |

Values round-trip through the existing bounded JSON↔Lua converters, so a row
table looks just like `json_parse()` output. Errors surface as ordinary Lua
errors carrying the operation name and the PostgreSQL error message — never
the connection URL, never the full SQL text.

`postgres.*` is available in `crew.lua` and `config.lua` (the same sandbox
IronCrew calls the "crew sandbox" elsewhere in these docs) and is **not**
available inside `tools/*.lua`'s `execute` function. See
[Sub-flow limitation](#sub-flow-limitation-run_flow) below for the one other
place it is deliberately absent.

## Limits (fail-closed, hard ceilings)

Same idiom as every other limit in IronCrew: invalid values fail startup.

| Env var | Default | Hard ceiling |
|---|---|---|
| `IRONCREW_APP_DB_MAX_CONNECTIONS` | 4 | 32 |
| `IRONCREW_APP_DB_STATEMENT_TIMEOUT_MS` | 5000 | 60000 |
| `IRONCREW_APP_DB_MAX_ROWS` | 500 | 10000 |
| `IRONCREW_APP_DB_MAX_RESPONSE_BYTES` | 1 MiB | 16 MiB |
| `IRONCREW_APP_DB_MAX_PARAM_BYTES` | 1 MiB | 16 MiB |
| `IRONCREW_APP_DB_MAX_OPERATIONS` (load-time) | 64 | 256 |
| `IRONCREW_APP_DB_MAX_SQL_BYTES` (per op, load-time) | 64 KiB | 1 MiB |

Concurrency is bounded by the pool size plus its acquire timeout. Exceeding
`IRONCREW_APP_DB_MAX_ROWS` on a `query` call fails with a hint to add a
`LIMIT`; exceeding `IRONCREW_APP_DB_MAX_RESPONSE_BYTES` fails mid-stream.

## Result decoding and the cast-hint rule

Result columns decode into JSON-compatible Lua values from a fixed set of
PostgreSQL types:

- Text family: `TEXT`, `VARCHAR`, `BPCHAR`, `CHAR`, `NAME`
- Integers: `INT2`, `INT4`, `INT8`
- Floating point: `FLOAT4`, `FLOAT8`
- `BOOL`
- `JSON` / `JSONB` (decoded natively — no extra parsing step)
- `NULL` (any of the above, when the value is absent)

A column of any other type — `timestamptz`, `uuid`, `numeric`, and so on —
fails the call with a hint to cast it in SQL rather than being silently
coerced or dropped. Cast the column in the declared operation instead of
changing the client:

```sql
-- instead of: SELECT created_at FROM checkpoints WHERE ...
SELECT created_at::text FROM checkpoints WHERE ...
```

## At-least-once execution and idempotent upserts

The documented contract is **at-least-once**, not exactly-once. Task
retries and `foreach` re-runs are core engine behavior, and the `postgres.*`
runtime does not fake a stronger guarantee than the rest of IronCrew provides
— consistent with the multi-replica honesty rules in `AGENTS.md`.

In practice this means: write operations should be `ON CONFLICT` upserts
keyed on an `idempotency_key` derived from stable identifiers, so a retried
task cannot duplicate rows. The `save_checkpoint` example above derives its
key as `execution_id || ':' || stage` from the SQL side; the equivalent Lua
pattern is:

```lua
local idempotency_key = execution_id .. ":" .. stage
```

Design every `execute` operation that writes data as an upsert (or another
naturally idempotent statement) rather than a plain `INSERT`, unless the
table's own constraints already make retries safe.

## Sub-flow limitation (`run_flow`)

**`postgres.*` is not available inside `run_flow` sub-flows in v1.** Calls
made from a sub-flow VM — whether invoked via `run_flow(path, input)` or
`crew:subworkflow(...)` — fail closed with:

> `postgres.* is not available inside run_flow sub-flows in this version;
> perform app-database operations in the parent flow and pass results in via
> input`

This was discovered during implementation: sub-flow VMs are built by a
separate setup path from the top-level crew VM, and it was not in scope to
thread the live app-database handle across that boundary in v1. If a flow
needs both sub-flow delegation and app-data checkpoints, do the
`postgres.*` calls in the parent flow and pass whatever the sub-flow needs
(or produces) through `input` / the sub-flow's return value. Threading the
live capability into sub-flow VMs is a tracked follow-up, not something to
work around by restructuring flows today.

## Configuration and fail-closed behavior

- **Enable it:** set `IRONCREW_APP_DATABASE_URL` to a PostgreSQL connection
  string. Use a dedicated role and schema — do not reuse the role behind
  `DATABASE_URL`.
- **Unset:** every `postgres.*` call fails with a configuration hint
  referencing `IRONCREW_APP_DATABASE_URL` instead of a raw nil-index error.
- **Built without the `postgres` cargo feature:** `postgres.*` calls fail
  with a message explaining the binary was built without PostgreSQL support.
  Setting `IRONCREW_APP_DATABASE_URL` on such a binary fails startup
  validation instead of silently doing nothing.
- **Pool connects lazily.** A flow that never calls `postgres.*` pays no
  connection cost even when the URL is configured.

## Example

See [`examples/postgres-checkpoints`](../examples/postgres-checkpoints) for a
runnable flow that checkpoints task output through an upsert operation and
reads it back.
