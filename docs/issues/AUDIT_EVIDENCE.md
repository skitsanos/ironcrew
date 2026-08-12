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

On 2026-08-11, the active runtime, scaffold, demos, and IC-009 plan moved to
`gpt-5.6-luna`; the July GPT-4.1 receipt remains historical rather than being
rewritten. Bounded real-endpoint text/strict-JSON and image-input probes passed
after GPT-5 Chat requests adopted `max_completion_tokens` and Luna-backed flows
removed unsupported explicit temperatures. Those probes establish API/runtime
compatibility only. They are not a repeated Luna effectiveness result, and the
full five-repetition synthetic run plus two independently reviewed intended-use
domain packs remain outstanding under IC-009.

On 2026-08-12, that bounded gap closed. The frozen GPT-5.6 Luna plan completed
180/180 local live-provider runs across 12 cases, five repetitions, and three
topologies, with 60 matched pairs per crew candidate and no execution, parse,
or schema failures. The DAG raised mean grounded correctness from 0.6500 to
0.7833; its +0.1333 paired delta had a Bonferroni-adjusted 97.5% interval of
[0.0500, 0.2167], and its 3.2113x token and 4.5645x latency multipliers stayed
inside the predeclared 6x ceilings. It therefore met every check and was the
bounded recommendation. The collaborative variant reached 0.7333 but did not
qualify because its interval crossed zero and its 6.7050x latency multiplier
exceeded the ceiling. `crew_qualified` means that the DAG qualified for this
plan; it does not mean every crew topology qualified.

Provider-reported usage was 289,816 prompt, 107,821 completion, zero cached,
and 397,637 total tokens. The frozen-price observed estimated upper bound was
$0.2018392, versus a $2.7528 planned bound and $3 approval budget; this is a
conservative token-derived estimate, not an invoice. The aggregate corpus,
plan, flow, and release-binary hashes are retained in the dated JSON receipt,
which also proves the dirty source manifest and binary stayed unchanged during
the run. The six-case synthetic core and two independently reviewed
representative synthetic intended-use packs are not production samples. This
is one operator-declared OpenAI API/GPT-5.6 Luna configuration running locally,
not multi-model, production-data, deployed-platform, or broad-superiority
evidence. The July GPT-4.1 receipts remain unchanged historical evidence.

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

The committed, not-yet-released implementation extends the authenticated
`/metrics` response with
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

## Retention-boundary replica soak closure — 2026-08-11

IC-018's predeclared provider-free contract passed with two direct release
processes sharing an isolated PostgreSQL 15.18 database. The 1,800.651-second
workload completed 2,613/2,613 keyed cross-replica HITL/SSE runs with zero
readiness, liveness, workload, or deadlock failures. Sixty-one observations
crossed the configured 600-second journal-retention boundary; 40 were
post-boundary, and the final 20 intervals covered 602.081 seconds with 7,322
latency samples and no declared violation.

Maximum retained journal state was 3,696 rows/3,784,704 bytes against
8,192/8 MiB ceilings. Expired physical rows peaked at 5 against 128, and
post-prune retained growth was zero rows/bytes against 512/1 MiB. Retention
state and PostgreSQL delete statistics advanced in 41 and 40 intervals. Prefix
delete deltas are correlated aggregate evidence rather than per-batch causality.
Prefix relations peaked at 11,984,896 bytes, WAL reached 69,072,576 bytes,
replica RSS peaked at 17,924,096/17,989,632 bytes, and tail RSS growth was
16,384/0 bytes. The required expired cursor, explicit retention gap, incomplete
synthesized terminal, and zero-row replay anchor all passed.

A separate bounded loopback-mock receipt passed exact provider/tool counts, an
exact 65,536-byte result, and a two-turn warm-owner/cold-peer committed
conversation boundary with matching history and the PostgreSQL conversation
SSE `409`. Paid-provider calls and estimated cost were zero. That receipt
explicitly records `in_flight_takeover_proven: false`; it is not live-provider
or arbitrary exactly-once external-effect evidence.

The long-soak and profile JSON receipts hash to
`sha256:750be5c83ebd4f5df2299c5b3a4b9483b06cdae50a9fc46b18906cdee49eab4e`
and
`sha256:2e00f1f7deca155cebbecc5adf65a36d0cec3f21a6cfb7191d49d3066a8e7d83`.
Both bind revision `fe906f50e37640adb00b37a846be648b3a1178f9`, the same
release-binary hash, and separately stable dirty-worktree manifests. The serial
live-PostgreSQL regression gate passed 48/48 store, 2/2 multi-replica HTTP, and
10/10 process tests against freshly pulled PostgreSQL 15.18 at digest
`sha256:6eb0add3b77c081df18aa518ce43df58fdcc40f2e6d868a6fd08038dc7acd425`.

The JSON receipts prove clean process exits and exact prefix cleanup to zero.
Separate fixture evidence confirms that the captured labeled `--rm`
PostgreSQL container was removed without a broad prune. Raw logs were not
retained. This closes IC-018's provider-free local-process evidence gap. No
live-provider, Railway, OpenShift, load-balancer, cgroup/OOM, autoscaling, or
production-monitoring result is claimed; the dirty artifacts were not published.

## Release-image promotion checkpoint — 2026-08-12

The IC-015 implementation moves multi-platform image construction
into the exact tag's release workflow. It produces one signed OCI archive and
a strict signed receipt bound to the tag commit, checksummed release binaries,
Dockerfile, content-addressed Wolfi base index, OCI index and platform objects,
and builder. The separately authorized Docker publisher verifies those assets
and promotes the archive without rebuilding current default-branch source.
Version promotion is absent-or-identical only; a conflicting existing digest
fails closed. Shared workflow coordination and bounded post-write GitHub
release revalidation cover the repository-side `latest` race between releases
that carry the complete signed IC-015 asset set.

Initial read-only GitHub and Docker Hub API checks on 2026-08-12 found GitHub stable
release `v2.22.0` (published 2026-07-07), but Docker Hub `latest` still matched
`2.20.0` at
`sha256:fa336f85a0347001438d576f2e945136eb40485f7a6a0355a77ea0dbf38230c6`;
the `2.21.0` and `2.22.0` tag endpoints returned `404`. Docker Hub also
reported immutable tags disabled, with the inactive rule `.*`.

Later live remediation enabled the exact stable-semver-only rule on
`skitsanos/ironcrew`; `latest` remains mutable. No production image or tag was
published or moved, and the pre-existing tag snapshot stayed unchanged. A
uniquely named disposable Docker Hub repository then passed initial promotion,
identical replay, conflict refusal, direct immutable-tag enforcement, second
version publication, and a two-attempt `latest` repair. Same-archive mutable
`latest` write/restore controls ruled out an unrelated copy failure as the
immutable-version rejection cause. Authenticated API and
registry checks proved the repository and all three acceptance tags absent
after exact UI cleanup. The sanitized retained receipt is
[`2026-08-12-ic015-dockerhub.json`](../../evaluations/release-acceptance/reports/2026-08-12-ic015-dockerhub.json),
SHA-256
`6635719fcda499cadc3a076182f7c7ab3b00cd19f362edbcd6781636bc9e2a11`.
The implementation and acceptance receipt landed in commit
`8bccba9b1c19f2deb9bd4353406b4623ebfeab14`. After a bounded test-only
stabilization for IC-017's minimum journal-read deadline, exact-head commit
`56fd1d96d3ae1f78ea92ed1590643e434f7cb98b` passed all nine jobs in
[CI run 31596627801](https://github.com/skitsanos/ironcrew/actions/runs/31596627801),
including PostgreSQL integration and replica-soak smoke. IC-015 is resolved;
production Docker publication remains deferred under IC-014 and the user's
release-last sequence.

This is a next-release protocol, not a historical-image backfill: the current
legacy `v2.22.0` release has none of the new OCI/receipt assets. A newer release
created outside the trusted workflow, or observed before its complete signed
asset set exists, fails closed; preventing that authority-level event remains
IC-014's platform boundary.

## Trusted release-control checkpoint — 2026-08-12

The in-progress IC-014 source checkpoint replaces release tag-push and
branch-selectable manual triggers with versioned `repository_dispatch` events.
GitHub selects these workflows from the default branch. The request validator
accepts an exact stable tag and `validate` or `publish` mode, rejects extra or
ambiguous payload fields, and binds the event to the repository and triggering
actor. Release selection separately requires a direct annotated tag-to-commit
reference, matching manifest version, `main` ancestry, and the same resolved
commit before final publication. The trusted workflow identity is
`release.yml@refs/heads/main`, while the strict image receipt retains the exact
tag and commit source binding.

Both release and Docker workflows now have a non-publishing `validate` path
through an environment named `release`. That path has contents-read permission
and no release, OIDC-signing, Docker-secret, or registry operation. Image build,
receipt, and SBOM packaging remain in a contents-read job. The final protected
release job only independently verifies the expected artifacts, signs them,
and creates the release once; only the Docker promotion job references Docker
credentials. These are local workflow and policy-test properties, not proof of
remote enforcement.

Initial read-only GitHub API checks on 2026-08-12 found zero repository
rulesets, zero environments, and no `main` protection object (`404`). The sole
direct collaborator was owner `skitsanos` with administrator access, so no
independent release reviewer currently exists. Actions allowed all actions, did
not require full-SHA pinning, used read as the default workflow permission, and
did not allow workflows to approve pull-request reviews.
`DOCKERHUB_USERNAME` and `DOCKERHUB_TOKEN` remained repository-scoped Actions
secrets. Immutable GitHub releases were initially disabled (`enabled: false`,
`enforced_by_owner: false`). The new preview workflow-execution-policy control
was not available as authoritative stable-API evidence and its state remains
unverified.

Default-branch dispatch selection does not prevent a tag-capable actor from
placing a separate tag-push workflow in an off-main commit. GitHub's preview
workflow-execution protections can constrain actors and events before workflow
execution, but no live policy was verified. Existing CI also uses push events
on `main` and `develop`, so a blanket push denial would require an agreed CI
trigger change. Repository dispatch is not itself least privilege: GitHub
requires Contents write to create it, and sender/actor equality is only a
consistency check. Closure needs a constrained release App/authority or a lower-
authority request channel backed by trusted platform controls.

There is also a tag time-of-check/time-of-use boundary: final tag revalidation
and `gh release create --verify-tag` are not atomic. Active `v*` rules must
restrict creation and prevent update/deletion without administrator bypass, and
immutable GitHub releases must be enabled, before the exact tag can be treated
as stable through publication.

Live remediation later on 2026-08-12 enabled immutable GitHub releases. The API
now reports `enabled: true` and `enforced_by_owner: false`; this applies to
future releases and did not retroactively change legacy release `v2.22.0`,
which still reports `immutable: false`.

Active repository tag ruleset `20741649`, named
`Lock v* release tags pending trusted publisher`, now targets
`refs/tags/v*` with creation, update, and deletion restrictions. It has no
bypass actors, and the API reports that the current user can never bypass it.
This deliberately fail-closed interim state prevents release-tag creation until
a constrained trusted publisher is available; it is not the final release-role
configuration.

A real push of canary tag `v0.0.0-ic014-lock-canary-20260812` was rejected by
GitHub with `GH013`. A follow-up ref lookup confirmed the remote tag is absent.
No release, image, workflow run, environment, `main` rule, collaborator, secret
scope, or registry state was created or changed by that canary.

IC-014 remains in progress until reviewed default-branch and
workflow-execution controls, a protected environment, and constrained request
authority exist; an independent approver and bypass policy are documented; the
repository-scoped Docker credentials are moved into that environment; and one
successful protected `validate` request is captured. The UI-only workflow-
execution-policy state remains unverified. The interim tag ruleset must be
converted to a least-privilege trusted-publisher policy without weakening
update/deletion protection. Release and Docker publication remain prohibited in
the meantime.

## Current evidence boundaries

The following remain explicitly unproven, incomplete, or unsupported. Tracked
gaps retain their issue-registry owners:

- execution takeover/checkpoint resume and exactly-once arbitrary external
  provider/tool effects, neither of which follows from replica routing;
- live-provider and Railway/OpenShift retention-boundary resource profiles;
  IC-018's predeclared provider-free local-process contract is green, but it is
  not platform or production steady-state proof;
- an attributed Railway load-balancer canary and a published release for
  arbitrary-routed keyed conversation turns. IC-008's local PostgreSQL and
  OpenShift dirty-artifact results are green, but Railway remains unrun;
  in-flight turn takeover and durable PostgreSQL conversation SSE remain
  unsupported;
- production-sample, multi-model, and deployed-platform crew-effectiveness
  generality beyond IC-009's bounded local GPT-5.6 Luna result;
- deployment-specific authenticated per-pod metrics collection, a hosted
  telemetry backend/dashboard, and billing remain external operator concerns;
- a complete platform-enforced trusted release control plane; IC-014's local
  default-branch dispatch and validation paths are implemented, immutable
  releases are enabled for future releases, and an active fail-closed `v*` tag
  ruleset rejected a real creation canary. The remote environment, independent
  reviewer, protected/default-branch and verified workflow-execution policy,
  constrained request authority, successful protected validation run, and
  environment-level secret scoping remain absent.
