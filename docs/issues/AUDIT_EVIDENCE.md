# IronCrew audit evidence

These snapshots preserve cross-issue baselines and reviewed measurements. They
are historical evidence, not a substitute for the current code, issue pages,
or deployment-specific acceptance.

## Deep Rust, documentation, examples, and resource audit

The July 2026 audit covered `src/`, public documentation, Lua examples, the
REST/CLI contracts, security boundaries, and Railway/OpenShift resource
guidance. The completed default gate included:

- `cargo build`
- `cargo fmt --all -- --check`
- `cargo check --all-targets`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test --all-targets`
- `./scripts/check-lua-examples.sh`: 66 Lua files plus 4 offline runtime probes

Release compilation with LTO and one codegen unit used roughly 1.5 GiB peak
compiler RSS on Apple Silicon. That is build-time evidence, not a pod runtime
requirement. Runtime sizing must instead multiply bounded Lua VMs, provider and
tool buffers, event journals, conversations, database connections, and active
work by the configured per-pod concurrency and replica count.

## Crew-effectiveness evidence

On 2026-07-19, an exploratory GPT-4.1 smoke compared a one-call baseline, a
three-call DAG, and a four-call collaborative crew on three synthetic grounded
decision cases with one repetition. Every variant chose the correct option IDs;
the crew variants improved evidence completeness in that small sample while
using materially more tokens and latency.

This is positive exploratory evidence, not broad proof that crews outperform
simpler workflows. Contract mode uses an oracle-backed mock provider and is
only evidence that orchestration, reporting, and scoring behave as designed.
IC-009 owns the repeated six-case and representative-domain evidence gap.

## Two-process PostgreSQL replica evidence

On 2026-07-19, two independent `ironcrew serve` processes sharing PostgreSQL
15 passed the keyed replay/conflict, cross-replica cancellation, encrypted HITL,
durable SSE replay, readiness, and graceful-shutdown acceptance path. The
associated 150-second provider-free soak completed 253/253 cross-replica
HITL/SSE runs without a readiness failure or deadlock.

On 2026-07-20, the owner-death extension passed 1/1 in 17.90 seconds. It sent
actual Unix `SIGKILL` to a process whose keyed run was durably
`WaitingForInput`; the surviving process reconciled the row to `Abandoned`
after the six-second database-clock lease expired. Same-key retries before,
during, and after reconciliation kept the original run/owner, while exact run
and event row counts stayed unchanged and the HITL mailbox was cleared.

This proves no second durable IronCrew execution for that retained principal,
key, and request within the tested idempotency window. It does not make
arbitrary external provider/tool effects exactly once.

Temporary deployed canaries on 2026-08-10 then observed two identities through
both Railway and an affinity-disabled OpenShift Route. Short provider-free v2
target runs passed 8/8 and 20/20 shared HITL/SSE cases respectively. Railway
also passed replay, conflict, cancellation, auth, and protected-metrics probes;
OpenShift passed replay, conflict, cancellation, and authentication probes plus
a real owner `SIGKILL`/`Abandoned` gate and the staged encrypted-HITL key
rotation through new-only. These were disposable
canaries, not long-duration production monitoring or provider/resource-ceiling
evidence. Attributable table prefixes and active platform resources were
removed afterward; at that interim checkpoint, Railway still exposed one
asynchronous pending-deletion volume record.

A follow-up IC-020 gate added monotonic replica draining, exact PostgreSQL
owner fences, authenticated lifecycle/resource metrics, and bounded local
`R=1/2/3` capacity evidence. Temporary Railway and OpenShift canaries then
passed `1 -> 2 -> 1`, direct/peer draining-control rejection, clean
signal-driven terminalization, and same-key replay. Railway continued routing
to its drained process, so the application fence—not readiness withdrawal—is
the safety boundary there. OpenShift's first rollout exposed a contention-only
readiness flap; bounded singleflight plus `minReadySeconds: 10` corrected it,
after which the affinity-free shared Route passed 180/180 readiness, liveness,
and capability probes during a homogeneous rollout. A retiring direct route
was not zero-gap. Both platform stacks and database prefixes were removed and
their attributable baselines restored.

The authoritative OpenShift IC-007 v7 canary then repeated the complete
applicable matrix with an independently attested dirty-worktree artifact. An
affinity-free Route returned 64/64 capability responses across two verified
pod/process identities (33/31). Counted replay/conflict, encrypted HITL,
numbered SSE and cursor edges, cancellation/race behavior, local admission,
shared quota, real owner `SIGKILL` and replacement, staged key rotation, and
explicit drain/replacement passed. The seven-phase rotation rerun retained 14
complete process inventories. Unkeyed run control and live conversation
message/SSE returned their intended process-local boundaries rather than false
success.

The OpenShift result is temporary platform evidence, not a release, long soak,
or reproducible/downloadable artifact claim. Exact scans found no canary
credentials or HITL plaintext, but the receipt retains five unfixed
HIGH/CRITICAL operating-system findings and the shared namespace's additive
same-namespace ingress allowance. Cleanup returned both exact selectors, all
three attributable database prefixes, and quota use to zero while preserving
the namespace baseline and authorized OAuth session.

Railway v7 then passed the literal remaining rotation requirement without being
misrepresented as a full matrix rerun. A retained overlap snapshot contained
two independently attested expanded/old-active and two expanded/new-active
processes. New answered old-owned work, old answered new-owned work, both runs
reached `Success` with complete seven-event barriers, and the scoped observer
captured one old and one new ciphertext fingerprint without fixed plaintext.
Observer objects and old references returned to zero before a final
two-process new-only peer run completed with the same barrier. Earlier retained
Railway receipts remain the separately dated evidence for routed replay,
conflict, cancellation, HITL/SSE, owner replacement, and lifecycle.

Railway rebuilt the verified ten-file v7 context, and every accepted process
matched its independently computed binary, flow, 113-field config, keyring,
helper, build-attestation, deployment, and process-start identity. The receipt
reports six database prefixes plus every active service, instance, domain,
proxy, volume, and bucket returned to zero; it also reports zero attributable
local staging/cache/scratch/Docker objects. The project, environment, two
pre-existing volume tombstones, and `postgres:15` remain. No broad delete or
prune was used. Independent final audit confirmed those cleanup facts, closing
IC-007's platform-evidence gap; a duplicate full Railway v7 matrix was not
invented as an added acceptance criterion.

The final local closure gate passed formatting, exact all-target Clippy, 969
Rust tests with 3 intentional ignores, doc tests, dependency audit over 431
packages, and a locked release build. Repository policy passed 3 skill checks,
20 registry tests, 21 Bun tests with 155 expectations, `actionlint`, worktree,
and diff validation. The Lua gate covered 66 files and 5 runtime probes;
Python passed 29 crew, 15 soak, 8 lifecycle, and 34 platform tests. The crew
contract completed 18 runs, 48 requests, and 36 grounded decisions. The live
PostgreSQL 15.18 gate passed 57/57 tests, the short soak passed 2/2, and all 3
lifecycle phases passed with exact cleanup. These local results do not replace
either platform receipt.

IC-008 then resolved the narrower committed-boundary conversation gap in the
reviewed, unpublished worktree. Two real `ironcrew serve` processes sharing
PostgreSQL 15.18 passed peer start, required-key message/replay, history,
same- and peer-process active-delete fencing, delete/recreate incarnation
fencing, restart, owner `SIGKILL` between turns, and the truthful PostgreSQL
conversation-SSE `409` boundary. The final serial PostgreSQL gate passed
60/60 tests, the release-binary two-process soak passed 2/2, and all exact
database/container/cache artifacts were removed while retaining the
`postgres:15` image.

The separately dated
[IC-008 OpenShift receipt](../../evaluations/platform-canary/reports/ic008-openshift.md)
then passed the applicable case-9 matrix through affinity-free Routes using an
independently attested but unpublished dirty-worktree artifact. Initial route
sampling was 64/64 with a 32/32 A/B split; replacement sampling was 32/32 with
a 16/16 B/C split. The canary retained exact replay/effect counts, history,
active-delete fences, delete/recreate incarnation fencing, the shared-store
conversation-SSE `409`, and a separate cold keyed recovery after the only
prior owner was force-deleted between committed turns. The human and machine
receipts hash to
`sha256:acff73fd9e7f6233a45c00791892813941ad4441e2d8e2810a3133502d098dcb`
and
`sha256:069848c1d1cc598743d9207350079b3a78919916c11e260202339b737315a6e4`.

This is OpenShift evidence, not a published/downloadable artifact or Railway
result. It does not prove in-flight Lua/provider/tool takeover, shared
conversation SSE, or general exactly-once effects. The shared namespace had an
additive same-namespace NetworkPolicy; deleted A/B final log tails and the
inline controller bytes were unavailable; and Docker Scout retained four
unfixed Debian-base HIGH/CRITICAL findings. Exact labeled objects, database
prefix/functions, quota, local staging/cache, and attributable Docker objects
returned to zero, with the namespace baseline restored at
`sha256:ce9697dfb8eb519641338240dcbb0ab328952ebc8b07c9500a511101d774d4dd`.

## Execution and storage metrics closure — 2026-08-11

The reviewed worktree extends the authenticated `/metrics` response with
fixed-cardinality, process-local counters and histograms for run, task, tool,
provider, provider-token, SSE, lease-loss, reconciliation, terminal-persistence,
and explicitly instrumented storage outcomes. Closed enums own every label;
caller-controlled identifiers, names, URLs, errors, content, and secrets cannot
enter the metric surface. Fixed cumulative duration buckets cover 5 ms through
300 seconds plus `+Inf`, and all counters, sums, counts, and buckets reset on
process restart.

The focused Rust evidence passed closed-vocabulary and exact-cardinality checks,
provider usage aggregation, cumulative histogram monotonicity during
concurrent record/scrape races, durationless abandoned-run accounting,
in-flight status exclusion, provider success/error/cancellation, and tool
error/cancellation without tool-name exposure. The real protected-endpoint test
retained authentication, `no-store`, existing-series compatibility, zero-valued
fixed combinations, and principal/token omission. Exact all-target Clippy and
the complete all-target Rust suite also passed:

- `cargo clippy --all-targets -- -D warnings`
- `cargo test --all-targets`

A disposable PostgreSQL 15.18 run additionally passed 48 PostgreSQL-store,
two multi-replica HTTP, and ten separate-process replica tests. A bounded
two-process soak completed two runs with no failures or deadlocks and observed
both replicas exactly 16 times across 32 capability probes. The dedicated
database container was removed afterward; the retained `postgres:15` image was
not pruned.

Publication remains behind the store-backed durable snapshot. A snapshot error
still returns `503` instead of stale/fabricated utilization, while its fixed
`metrics_snapshot` operation counter becomes visible after recovery. This is
local implementation and regression evidence, not proof that a hosted metrics
backend, dashboard, Railway/OpenShift per-pod scraper, billing pipeline, or
production alert policy exists. IC-010 is resolved on implementation, local
regression, disposable PostgreSQL, and bounded separate-process evidence; no
deployed per-pod scraper or release is claimed.

## Current evidence boundaries

The following remain explicitly unproven, incomplete, or unsupported. Tracked
gaps retain their issue-registry owners:

- execution takeover/checkpoint resume and exactly-once arbitrary external
  provider/tool effects, neither of which follows from replica routing;
- a retention-boundary steady-state soak with predeclared ceilings;
- an attributed Railway load-balancer canary and a published release for
  arbitrary-routed keyed conversation turns. IC-008's local PostgreSQL and
  OpenShift dirty-artifact results are green, but Railway remains unrun;
  in-flight turn takeover and durable PostgreSQL conversation SSE remain
  unsupported;
- broader, repeated crew-effectiveness evidence;
- deployment-specific authenticated per-pod metrics collection, a hosted
  telemetry backend/dashboard, and billing remain external operator concerns;
- an honest module-size baseline ratchet for legacy oversized Rust modules; and
- a platform-enforced trusted release control plane.
