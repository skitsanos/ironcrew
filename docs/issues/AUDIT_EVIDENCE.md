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
arbitrary external provider/tool effects exactly once, and it is local process
evidence rather than Railway or OpenShift routing evidence.

## Current evidence boundaries

The following remain explicitly unproven or incomplete and are tracked in the
issue registry:

- bounded maintenance query/advisory-lock waits and safe lease timing;
- kill-during-cancellation and separate-process unkeyed wrong-owner cases;
- real Railway/OpenShift routing without affinity;
- staged mixed-revision HITL key rotation;
- separate-process durable-SSE cursor/gap coverage and a retention-boundary soak;
- measured process-local versus shared admission saturation;
- cluster-wide admission and graceful drain/scale lifecycle evidence;
- distributed live conversation control;
- broader, repeated crew-effectiveness evidence;
- execution and storage-health metric coverage;
- an honest module-size baseline ratchet for legacy oversized Rust modules; and
- a platform-enforced trusted release control plane.
