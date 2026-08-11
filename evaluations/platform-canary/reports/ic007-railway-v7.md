# Railway IC-007 v7 platform canary

Status: staged Railway rotation and exact sandbox cleanup passed.

This short sandbox canary closes the remaining literal Railway part of IC-007:
IC-016's expanded/old-active, expanded/new-active, zero-old-reference, and
new-only sequence during a real rolling overlap. Existing retained receipts
remain the evidence for public routing, replay/conflict/cancellation,
encrypted HITL, retained SSE, owner replacement, and IC-020 lifecycle. Those
cases were not repeated as a full v7 matrix.

The complete machine receipt is `ic007-railway-v7.json`; the 113-entry
secret-safe configuration is `ic007-railway-v7-effective-config.json`.

## Artifact and configuration

Railway rebuilt the exact verified ten-file v7 assembly context. The observed
Railway image digest was
`sha256:78dd8d7adf24832cd2cf4b3a2020b0326899538a274d932da5a4f522b21124a1`.
Every accepted process independently recomputed the same binary
(`sha256:b80ec1f5...a9467e0`), flow tree (`sha256:7a66cd31...a4256`), build
attestation, 113-field configuration (`sha256:2fdce427...e6a2`), and all three
in-image helper hashes. These values matched authenticated `/capabilities` and
the target deployment-instance identity.

The Railway digest is intentionally distinct from the source OCI index and
Linux manifest. The receipts retain exact small context inventories and
digests, not a downloadable binary, OCI image, or bit-reproducibility claim.

The rotation-only profile used lease TTL 60 seconds, journal write timeout
5000 ms, HITL poll interval 5000 ms, Lua execution limit 300 seconds, database
pool size 2, and no SSE output-character override. Both final child processes
independently emitted the same complete non-secret manifest. The provider URL
was the Railway-private
`http://ic007-final-provider.railway.internal:8080/v1`; `PORT` was the
platform-injected value `8080` in every accepted process.

## Process and platform identity

The accepted mixed window bound two old and two new processes in one
before/after Railway deployment snapshot:

| Phase | Deployment | Instance | Process start | Keyring |
| --- | --- | --- | --- | --- |
| expanded/old | `f8f98254...dc83` | `461fc1b5...fecb` | `e07139d8...e93c` | `845d8ed5...9613` |
| expanded/old | `f8f98254...dc83` | `c56893e2...94a8` | `f4c94941...13a4` | `845d8ed5...9613` |
| expanded/new | `f192da61...44f5` | `585f3a1d...3138` | `37905c1b...87e9` | `4b8bed9d...d0cf` |
| expanded/new | `f192da61...44f5` | `b33c903d...af1a` | `960c7be7...1749` | `4b8bed9d...d0cf` |
| new-only | `914e6fec...4f21` | `021524f9...52af` | `a4d32b14...62e0` | `0573837f...7df1` |
| new-only | `914e6fec...4f21` | `2079b924...e59a` | `ae477290...be51` | `0573837f...7df1` |

The final deployment was `SUCCESS`, its exact two instances were `RUNNING`,
and its immutable manifest recorded top-level replicas 2, regional replicas 2,
1 CPU and 1,000,000,000 bytes per process, the supervisor command, health path
`/health/ready`, and rollout overlap 600 seconds. The supervisor was an
intentional rotation-only launch difference: PID 1 was Python UID 10001 with
exactly one UID-10001 IronCrew child whose executable and argv were verified.

The authoritative mixed peak was four app processes: 4 vCPU,
4,000,000,000 bytes, and at most eight configured PostgreSQL pool connections.
The final new-only transition briefly had six processes before exact predecessor
removal: 6 vCPU, 6,000,000,000 bytes, and at most twelve pool connections.
These are limit arithmetic, not sustained usage or OOM evidence.

## Staged encrypted HITL rotation

During the exact 2+2 overlap, old-owned run
`4ce89ee2-1e8b-476e-ac7b-0b39629549b8` was answered by a new-active process,
and new-owned run `3e31864d-a858-40ed-8e9c-31e34f0d2636` was answered by an
old-active process. Both cross-cohort first answers returned `202` with
`Cache-Control: no-store`, both second questions were also answered, and both
original owners reached `Success`. Each terminal journal barrier was exactly
`0|2|2|1|true|7`.

A prefix-scoped, SECURITY INVOKER PostgreSQL observer captured only the
registered pending-to-answered transitions inside the answer transactions.
It retained hashes and lengths rather than raw ciphertext. The two captures
proved distinct old/new material fingerprints, 12-byte nonces, absent fixed
plaintext, immutable question/routing fields, and one capture per target. Its
summary was targets 2, captures 2, old 1/1, new 1/1, unexpected labels 0. The
recorded-OID cleanup used no `CASCADE`, all four observer OIDs returned to zero,
and old question, answer, and union references were `0|0|0` before retirement.

The final new-only peer run `8c2c5c19-7479-484a-912b-66cf9a2f4a3a` then used
only the new material. Its two peer answers returned `202`/no-store, the
original owner remained unchanged, terminal status was `Success`, the mailbox
was empty, and the numbered journal barrier was `0|2|2|1|true|7`.

## Scans and boundaries

Unauthenticated liveness/readiness returned `200`; capabilities and metrics
returned non-cacheable `401`. Bounded application build/runtime, provider, and
v7-window PostgreSQL log exports checked eight exact credential/key patterns
and found zero matches. Railway classifies Rust stderr as error-level records;
the three accepted app runtime exports had zero semantic error-keyword lines.
The v7 PostgreSQL window had eight stderr-classified lines and zero semantic
error-keyword lines. Historical PostgreSQL logs still contain the deliberately
retained v5 negative evidence, so no platform-wide clean-log claim is made.

Before cleanup, every known prefix had zero pending or answered HITL rows,
observer objects were zero, prefixed sequences were zero, four application
connections were idle, and no session was blocked. The exact prefix row/status
inventory is retained in the JSON receipt.

`security_clean` is deliberately not claimed: the Railway-rebuilt image was
not independently vulnerability-scanned as a downloadable artifact. The
secret/log checks and runtime UID/resource assertions are narrower evidence.
This canary also does not prove exactly-once arbitrary external effects,
execution takeover, live conversation portability, or long-duration behavior.

## Excluded attempts

- v5 remains negative journal-timeout evidence; v6 remains cancelled preflight.
- The overlap-90 pair `c2382661...e8e4` / `8693d388...4a17` is excluded because
  only old-to-new compatibility was captured before the old cohort disappeared.
  Both runs terminalized, mailbox rows returned to zero, the partial observer
  was removed by exact OID, and old references were `0|0|0`.
- A zero-length read of the overlap-600 result file was an operator monitoring
  error while the one controller process was still running. The same single
  invocation later produced the retained 9,224-byte successful receipt; there
  was no duplicate invocation.

## Cleanup

Cleanup completed at `2026-08-10T20:44:38Z`. The six recorded prefixes were
dropped explicitly without `CASCADE`; the post-drop inventory was zero
relations, tables, indexes, sequences, and functions matching `ic007%`. Domain
`92d1008b...6fb1` and only the three recorded app/provider/PostgreSQL services
were deleted. The final project snapshot had zero services, service instances,
domains, TCP proxies, active volumes, and buckets. The sandbox project and
production environment remain, as do only the two pre-existing
pending-deletion volume tombstones `59b97c18...5637` and
`96da93ee...70c1` with their original deletion timestamps and no service.

No broad delete or Docker prune was used. Exact local scratch scripts, Python
cache, temporary evidence files, and the private ten-file staging context were
removed; no attributable IronCrew/IC-007 Docker container or image remained,
while the shared `postgres:15` image was retained. The staged binary/context
copy was removed with exact unlink operations and is not recoverable from this
workspace. The retained hashes prove observed identity only; this receipt does
not claim a downloadable or bit-reproducible artifact.
