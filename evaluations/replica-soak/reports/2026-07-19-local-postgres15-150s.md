# Local two-replica PostgreSQL soak — 2026-07-19

## Verdict

**Passed the bounded local process-isolation gate.** Two independent IronCrew
2.24.0 release processes shared PostgreSQL 15 for 150 seconds and completed
253/253 provider-free keyed runs. Every run crossed replicas for encrypted
HITL observation/answer delivery, durable SSE replay, and terminal run reads.
There were no HTTP workload errors, readiness/liveness failures, deadlocks,
process exits, or forced shutdowns.

This is not Railway or OpenShift production evidence. The run used loopback
HTTP on macOS and PostgreSQL in Docker. Per-process RSS was sampled from the
host; the 1 GiB Railway/OpenShift value was a non-enforced comparator, not a
pod cgroup limit. No claim is made about load-balancer routing, CPU throttling,
OOM/eviction behavior, or execution takeover after owner death.

The machine-readable report is
[`2026-07-19-local-postgres15-150s.json`](2026-07-19-local-postgres15-150s.json).
Its generated absolute binary path was normalized to the repository-relative
`target/release/ironcrew`; metric values are otherwise unchanged.

## Identity and workload

| Field | Value |
|---|---:|
| Source revision | `668f313dcccc634d556ff38527f49af7bd731988` (clean) |
| Binary SHA-256 | `e1d535c1394a5860129c8e8ff6c322dc219e7a6275e6d0d762894beec897a41e` |
| Host | Darwin 25.5.0, arm64 |
| PostgreSQL | 15.18, Docker; `pg_stat_statements` enabled |
| Runtime DB role | Non-superuser database owner with `USAGE, CREATE` on the isolated schema |
| Replicas | 2 release processes, distinct PIDs and instance ids |
| Workload | 150-second admission window, concurrency 2, 253 completed runs (1.68 runs/s) |
| External model/tool calls | 0 planned and 0 required by the fixture |
| Authentication/idempotency | Bearer authentication enabled; idempotency keys required and sent |
| Per-replica PostgreSQL pool / active-run cap | 2 / 2 |
| HITL owner poll / durable-read concurrency | 1000 ms / 2 per replica |
| Journal poll / per-run event cap | 500 ms / 200 |

## HTTP and control results

All counts below are 253 with zero errors unless stated otherwise.

| Operation | p50 | p95 | p99 | Max |
|---|---:|---:|---:|---:|
| Keyed run acceptance | 10.839 ms | 23.536 ms | 82.879 ms | 109.930 ms |
| Cross-replica question read | 5.457 ms | 10.031 ms | 45.311 ms | 49.930 ms |
| Cross-replica answer enqueue | 5.458 ms | 10.007 ms | 28.068 ms | 33.435 ms |
| Initial cross-replica SSE event | 510.297 ms | 516.087 ms | 540.107 ms | 560.974 ms |
| Cursor-resumed terminal SSE | 511.040 ms | 1026.874 ms | 1523.793 ms | 1531.414 ms |
| Terminal run read through peer | 1.169 ms | 2.827 ms | 18.930 ms | 20.449 ms |
| Liveness (287 probes) | 0.622 ms | 3.765 ms | 12.330 ms | 18.386 ms |
| PostgreSQL-aware readiness (287 probes) | 17.675 ms | 30.546 ms | 119.580 ms | 139.689 ms |

The SSE latency distribution matches the configured 500 ms journal poll and
1000 ms HITL poll boundaries. It is not evidence for a lower-latency event
transport.

## Memory

| Replica | First RSS | Peak/final RSS | Peak vs 1 GiB comparator | Last-30-second slope |
|---|---:|---:|---:|---:|
| A | 13.125 MiB | 16.781 MiB | 1.639% | 157 bytes/s |
| B | 13.109 MiB | 16.734 MiB | 1.634% | 0 bytes/s |

Most growth was startup/warm-up: both replicas gained about 3.5 MiB in the
first 30 seconds. Across the final 60 seconds, A grew 32 KiB and B grew 16 KiB;
across the final 30 seconds, A grew 16 KiB and B was flat. This short run shows
a plateau under this fixture, not proof against leaks in provider, tool, MCP,
conversation, large-result, or longer-retention workloads.

## PostgreSQL pressure and growth

| Signal | Observed delta | Per successful run |
|---|---:|---:|
| Prefix-scoped relation bytes (heap + indexes + overhead) | 2.008 MiB | 8.13 KiB |
| Database size | 2.141 MiB | 8.66 KiB |
| WAL | 4.743 MiB | 19.20 KiB |
| Retained journal accounting | 1,036,288 bytes / 1,012 events | 4,096 bytes / 4 events |
| Serialized journal payload | 155,595 bytes | 615 bytes |
| Estimated dead tuples at boundary | 912 | 3.61 |
| Autovacuum / autoanalyze runs | 9 / 16 | — |
| Deadlocks / temp bytes | 0 / 0 | — |

The shared statement counter recorded 3,411 human-input, 11,594 run-event,
5,519 run-record, and 7,171 other prefix-matching calls. These are measured SQL
statement executions for each broad table category, not one-to-one logical
poll counts. The runner separately issued exactly 253 question reads, 253
answer enqueues, 253 initial SSE connections, and 253 SSE reconnects. Its
interval-derived estimates were 173 owner HITL-read and 589 journal-poll
opportunities; those estimates are explicitly not database call counts.

PostgreSQL reported 285 rolled-back transactions. These align with the
aggressive 500 ms readiness sampling: the store health check intentionally
rolls back its bounded write probe. Production probe intervals should be
included in database-capacity estimates.

## Cleanup and confidentiality

- Both replicas exited with code 0 after SIGTERM; neither needed SIGKILL.
- Exact-prefix cleanup completed without `CASCADE`; a post-run query found zero
  remaining relations for `soak_397dd81c_`.
- The reviewed JSON contains no credentialed DSN, database password, bearer
  token/header, HITL key material/id, answer literal, or raw idempotency key.
- WAL and `pg_stat_database` counters are database-wide. This database was
  isolated for the run, but the report preserves that attribution caveat.

## Remaining gates

1. Run the process acceptance owner-death case: SIGKILL the owner, observe
   `Abandoned` after lease expiry through the peer, and prove same-key replay
   preserves the original run id without execution takeover.
2. Repeat this harness on temporary two-replica Railway Pro and OpenShift
   staging services through both direct replica addresses and the real
   load-balancer/Service path. Capture platform cgroup/pod memory, CPU
   throttling, restarts/OOM/evictions, and database service metrics.
3. Run a 30–60 minute provider/tool-free profile across the journal retention
   boundary, then add representative provider/tool and conversation profiles.
   The 150-second result does not establish a long-term storage steady state.

Subsequent evidence on 2026-07-20 closed item 1 at the local process level.
`tests/two_process_replica_acceptance_test.rs` passed 1/1 in 17.90 seconds
against isolated PostgreSQL 15 after sending `SIGKILL` to an active owner. The
surviving replica reconciled the original run to `Abandoned` after its real
six-second database-clock lease expired. Same-key retries spanned the
live-lease, expiry, and reconciliation boundaries, followed by four concurrent
post-reconciliation retries; all replayed the original acceptance while exact
durable run-row and run-event-row counts remained unchanged. This does not
retroactively make this 150-second soak an owner-death or platform run. Items 2
and 3 remain open.
