# Replica lifecycle capacity gate

This IC-020 gate launches one, then two, then three real `ironcrew serve`
processes against one disposable PostgreSQL 15 database. Each process is
directly addressed; no load balancer is inferred.

For every phase the harness admits two runs per process. Their single provider
call is held at a bounded loopback-only OpenAI-compatible fixture while the
harness:

- opens the declared two SSE streams per process and proves one additional
  stream per process returns `429`;
- checks each process's protected active-run, provider, SSE, EventBus, and
  PostgreSQL-pool metrics;
- samples per-process and aggregate host RSS;
- counts PostgreSQL connections excluding the observer connection; and
- measures exact retained journal rows, payload bytes, and accounted bytes.

After each phase, it closes the streams and boundedly requires active runs,
provider calls, SSE connections, EventBus instances, retained events/bytes, and
their configured capacities to return to zero before adding another process.
The receipt records the cleanup latency and exact per-process zero snapshot.

The mock accepts at most one MiB per request, records only fixed counters, and
never reads a live provider key. The flow asserts that its dedicated
`IC020_PROVIDER_BASE_URL` is explicitly allowlisted, while
`IRONCREW_ALLOW_PRIVATE_IPS=true` is confined to these loopback child
processes; it cannot silently fall back to a public provider URL.

## Frozen local envelope

| Resource | Per process | Aggregate at `R` processes |
|---|---:|---:|
| PostgreSQL pool | 2 | `2R` |
| Active runs / planned provider calls | 2 | `2R` |
| Live SSE | 2 | `2R` |
| Host RSS comparator | 256 MiB | `256R` MiB |
| Replay bytes | 512 KiB | `512R` KiB |
| Durable producer queue bytes | 512 KiB | `512R` KiB |

The final two rows are configured logical payload envelopes for two active
runs. Protected metrics also measure the current retained EventBus count and
approximate serialized payload bytes against those capacities. They are not
direct heap measurements and exclude Rust/container overhead. Broadcast
entries share their payload `Arc` with replay history. RSS is the observed
process comparator; PostgreSQL journal bytes are measured independently. The
shared journal is capped at 256 retained events, a 128-row prune batch, and 4
MiB logical accounted bytes, so those caps do not multiply with `R`.

Provider concurrency is a measured planning envelope, not a service-wide
semaphore. A trusted gateway remains responsible for a true cluster-wide
provider/API quota.

## Local run

Pull the newest patch of IronCrew's supported minimum major immediately before
the gate. The fixed name and captured ID make cleanup attributable; refuse to
reuse a pre-existing container and never prune Docker globally.

```bash
docker pull postgres:15

ironcrew_ic020_pg_name=ironcrew-ic020-capacity-pg
if docker inspect "$ironcrew_ic020_pg_name" >/dev/null 2>&1; then
  echo "refusing to reuse existing $ironcrew_ic020_pg_name" >&2
  exit 1
fi

ironcrew_ic020_pg_id=$(docker run --rm -d \
  --name "$ironcrew_ic020_pg_name" \
  -e POSTGRES_USER=ironcrew \
  -e POSTGRES_PASSWORD=ic020-capacity-local-password \
  -e POSTGRES_DB=ironcrew_capacity \
  -p 127.0.0.1:55433:5432 \
  postgres:15) || exit 1
test -n "$ironcrew_ic020_pg_id" || exit 1

cleanup_ic020_capacity() {
  current_id=$(docker inspect --format '{{.Id}}' "$ironcrew_ic020_pg_name" 2>/dev/null || true)
  if test -n "$current_id" && test "$current_id" = "$ironcrew_ic020_pg_id"; then
    docker stop "$ironcrew_ic020_pg_id" >/dev/null
  fi
  if docker inspect "$ironcrew_ic020_pg_id" >/dev/null 2>&1; then
    echo "IC-020 PostgreSQL container was not removed" >&2
    return 1
  fi
}
trap cleanup_ic020_capacity EXIT INT TERM

until docker exec "$ironcrew_ic020_pg_id" \
  pg_isready -U ironcrew -d ironcrew_capacity >/dev/null 2>&1; do
  sleep 1
done

cargo build --release --features postgres --bin ironcrew

python3 evaluations/replica-lifecycle/capacity.py \
  --binary target/release/ironcrew \
  --database-url postgres://ironcrew:ic020-capacity-local-password@127.0.0.1:55433/ironcrew_capacity \
  --postgres-container "$ironcrew_ic020_pg_id" \
  --report evaluations/replica-lifecycle/reports/2026-08-10-local-postgres15
```

The runner requires a loopback DSN and a running container whose configured
image is exactly `postgres:15`. It generates a unique
`ic020cap_<random>_` table prefix, stops only its own child process groups, and
drops/verifies only that prefix even after a failed phase. It does not stop the
caller-owned PostgreSQL container; the shell trap above does that by captured
container ID.

## Unit contract

```bash
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover \
  -s evaluations/replica-lifecycle \
  -p 'test_*.py'
```

The resulting JSON is the machine-readable receipt. Its Markdown companion is
the reviewed summary. Neither stores the database password, bearer token,
provider request bodies, idempotency keys, or raw process logs.
