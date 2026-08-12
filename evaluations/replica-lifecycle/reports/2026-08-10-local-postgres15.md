# IC-020 local replica-capacity evidence — 2026-08-10

**Result: PASSED**

One, two, and three real IronCrew processes stayed within the predeclared local envelopes while provider work and SSE were saturated.

## Evidence boundary

This is a local macOS/Linux host-process and disposable PostgreSQL 15 gate. The provider is a bounded loopback mock. RSS is sampled from host processes, not a pod cgroup, and this report is not Railway/OpenShift or live-provider proof.

## Predeclared ceilings

| Resource | Per replica | Aggregate at R replicas |
|---|---:|---:|
| PostgreSQL pool | 2 | `R × 2` |
| Active runs / planned provider calls | 2 | `R × 2` |
| Live SSE | 2 | `R × 2` |
| Host RSS comparator | 256.00 MiB | `R × 256.00 MiB` |
| Replay + durable-queue logical payload envelope | 1.00 MiB | `R × 1.00 MiB` |

EventBus retained bytes are measured as approximate serialized payload size; capacity is configured logical payload capacity, not heap/RSS. Both exclude Rust metadata, and the broadcast ring shares `Arc` payloads with replay history. PostgreSQL journal bytes are measured independently.

## Phase results

| R | PG conns peak / ceiling | Provider peak / plan | SSE open / ceiling | Extra SSE rejects | EventBus retained / capacity | RSS peak / comparator | Journal rows / accounted bytes |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 2 / 2 | 2 / 2 | 2 / 2 | 1 / 1 | 632 / 524288 bytes | 16.88 MiB / 256.00 MiB | 10 / 10240 |
| 2 | 4 / 4 | 4 / 4 | 4 / 4 | 2 / 2 | 1264 / 1048576 bytes | 34.28 MiB / 512.00 MiB | 30 / 30720 |
| 3 | 6 / 6 | 6 / 6 | 6 / 6 | 3 / 3 | 1896 / 1572864 bytes | 51.77 MiB / 768.00 MiB | 60 / 61440 |

Each phase held exactly two provider calls and two SSE streams per process while sampling. Every process reported its local active-run/SSE gauges at the configured limit; one additional direct SSE request per process returned 429.

## Bounded post-phase quiescence

Before scaling to the next process count, the gate closed every SSE stream and boundedly waited for active runs, provider calls, SSE connections, EventBus instances, retained events/bytes, and their configured capacities to reach zero.

| R | Cleanup latency | Replicas checked | Exact zero snapshot |
|---:|---:|---:|---:|
| 1 | 948.321 ms | 1 | `true` |
| 2 | 956.093 ms | 2 | `true` |
| 3 | 925.288 ms | 3 | `true` |

## Reproducibility and cleanup

- Git commit: `c4799a3c3b8a2441243ad512436d1cb649275cf4` (dirty worktree: `true`)
- Binary SHA-256: `3649b92f714a01140d2392ee1313e64a71285393d5f6cf49da47094ca7886277`
- PostgreSQL: `15.18 (Debian 15.18-1.pgdg13+1)` using the moving `postgres:15` contract
- Exact prefix cleanup: `{"prefix": "ic020cap_ff4645b5_", "remaining_functions": 0, "remaining_relations": 0}`
- Controlled replica exits: `{"replica-1": {"exit_code": 0, "forced_kill": false}, "replica-2": {"exit_code": 0, "forced_kill": false}, "replica-3": {"exit_code": 0, "forced_kill": false}}`
