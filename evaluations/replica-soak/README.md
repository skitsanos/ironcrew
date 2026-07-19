# Two-replica PostgreSQL soak

This harness measures two independent IronCrew HTTP processes sharing one
PostgreSQL schema. Its fixture stops at `crew:ask_human()` and returns after the
answer; it never calls `crew:run()`, so the workload plans **zero LLM calls**.
The child environment also points OpenAI at an unreachable loopback port as a
fail-closed guard.

The defaults mirror the conservative OpenShift baseline used for IronCrew:

- PostgreSQL pool: 2 connections per replica
- active runs: 2 per replica
- owner HITL poll: 1000 ms
- concurrent HITL PostgreSQL reads: 2 per replica
- retained events: 200 per run
- idempotency keys: required for run admission
- memory comparator: 1 GiB per replica

The 1 GiB value is only a pass/fail **comparator**. The runner does not enforce
a local memory limit and must not be described as reproducing Railway or
OpenShift OOM behavior.

## Local 2-3 minute run

Use a dedicated PostgreSQL container and a release binary for meaningful RSS
and latency evidence:

```bash
docker run --rm -d --name ironcrew-replica-soak-pg \
  -e POSTGRES_USER=ironcrew \
  -e POSTGRES_PASSWORD=ironcrew \
  -e POSTGRES_DB=ironcrew_soak \
  -p 55432:5432 postgres:17

cargo build --release --features postgres --bin ironcrew

DATABASE_URL='postgres://ironcrew:ironcrew@127.0.0.1:55432/ironcrew_soak' \
python3 evaluations/replica-soak/soak.py \
  --postgres-container ironcrew-replica-soak-pg \
  --binary target/release/ironcrew \
  --runs 200 \
  --duration-seconds 150 \
  --concurrency 2
```

`--duration-seconds` stops admitting new runs; already-started requests finish
under their bounded endpoint timeouts. The generated table prefix is deleted
after the observations are captured. Add `--keep-database` to retain it.
The SSE timeout is a socket-inactivity and complete-line bound: an adversarial
server that continuously trickles an unterminated line could extend it, so
target mode is intended only for trusted IronCrew replicas.
Stop the PostgreSQL container separately when finished:

```bash
docker stop ironcrew-replica-soak-pg
```

For a wiring smoke:

```bash
DATABASE_URL='postgres://ironcrew:ironcrew@127.0.0.1:55432/ironcrew_soak' \
python3 evaluations/replica-soak/soak.py \
  --postgres-container ironcrew-replica-soak-pg \
  --binary target/release/ironcrew \
  --runs 2 --duration-seconds 30 --concurrency 1
```

The report path is printed on stdout. Reports and replica logs default to
`evaluations/replica-soak/reports/` and are gitignored.

## Existing replicas / Railway / OpenShift

Target mode never launches or stops the services and never cleans their schema
unless `--cleanup-database` is explicitly supplied:

```bash
export DATABASE_URL='postgres://...'
export IRONCREW_API_TOKEN='...'

python3 evaluations/replica-soak/soak.py \
  --mode target \
  --base-a https://replica-a.example.test \
  --base-b https://replica-b.example.test \
  --table-prefix production_soak_ \
  --runs 50 --duration-seconds 120
```

Run against an isolated database or quiet evaluation schema. WAL and
`pg_stat_database` counters are database-wide and can include unrelated work.
The table, index, exact-row, dead-tuple, and autovacuum observations are scoped
to the supplied prefix.

Remote HTTP access alone cannot expose pod RSS. Supply `--pid-a/--pid-b` only
when the harness can actually read those host PIDs. On a Linux Docker host,
`--docker-container-a/--docker-container-b` resolves container init PIDs. On
Docker Desktop for macOS those Linux PIDs live inside the VM and host-process
RSS remains unavailable. For Kubernetes, per-process RSS generally requires
running in the same PID namespace; pod/cgroup memory should instead come from
the platform metrics pipeline.

## PostgreSQL observation backend

The Python runner has no third-party dependencies. It needs either:

- `psql` on `PATH` (managed PostgreSQL and normal local use), or
- `--postgres-container NAME`, which runs `psql` in a local PostgreSQL
  container through `docker exec`.

The report includes:

- exact per-table row counts and heap/index/total relation bytes;
- per-index size and scan-counter growth;
- journal logical payload bytes and conservative `accounted_bytes`;
- WAL-position delta and database-wide transaction/buffer/tuple counters;
- dead-tuple estimates, autovacuum/autoanalyze counters, and last timestamps;
- optional prefix-filtered `pg_stat_statements` call/block/time deltas;
- per-operation HTTP p50/p95/p99, status/error counts, and SSE bytes;
- exact client question polls/SSE connections plus explicitly labelled derived
  server poll opportunities;
- sampled host-process RSS, Linux `VmHWM` where available, and separately
  labelled cgroup memory.

`pg_stat_statements` is optional. Its section says why it is unavailable when
the extension or permissions are missing.

## Pass criteria and secret handling

`status: "passed"` requires:

- at least one run and zero failed runs;
- zero health-probe errors;
- both locally launched replicas still alive before controlled shutdown;
- zero PostgreSQL deadlock delta with an unchanged statistics reset boundary;
- every available sampled host-process RSS peak below the configured
  comparator.

The comparator remains non-enforced even though it participates in evaluation.
Cgroup memory is not substituted for per-process RSS because a pod/container
cgroup can include multiple processes.

The JSON report stores no database DSN, bearer token, HITL key material, answer,
or raw idempotency key. Database labels omit credentials. The workload only
records run IDs and bounded error summaries.

## Tests

```bash
python3 -m unittest discover \
  -s evaluations/replica-soak \
  -p 'test_*.py'
```
