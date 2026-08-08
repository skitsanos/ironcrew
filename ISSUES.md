# IronCrew engineering issues

The canonical engineering ledger is maintained in
[`docs/issues/README.md`](docs/issues/README.md). Individual findings use
stable paths such as [`docs/issues/IC-001.md`](docs/issues/IC-001.md).

## Active findings

| ID | Priority | Status | Area | Summary |
|---|---:|---|---|---|
| [IC-007](docs/issues/IC-007.md) | P1 | Open | Deployment validation | Railway and OpenShift replica routing lacks platform evidence |
| [IC-008](docs/issues/IC-008.md) | P1 | Open | Distributed conversations | Live conversation control remains process-owned |
| [IC-009](docs/issues/IC-009.md) | P1 | Open | Product evidence | Crew effectiveness evidence is too small for product claims |
| [IC-010](docs/issues/IC-010.md) | P1 | Open | Observability | Execution and storage metrics are incomplete |
| [IC-011](docs/issues/IC-011.md) | P2 | Open | Maintainability | Legacy oversized Rust modules lack a growth ratchet |
| [IC-014](docs/issues/IC-014.md) | P1 | Open | Release security | Release publication lacks a platform-enforced trusted control plane |
| [IC-016](docs/issues/IC-016.md) | P1 | Open | Replica encryption | Staged HITL key rotation lacks a replica acceptance gate |
| [IC-017](docs/issues/IC-017.md) | P2 | Open | Replica event replay | Durable SSE edge cases lack a separate-process gate |
| [IC-018](docs/issues/IC-018.md) | P2 | Open | Replica capacity | Replica soak lacks retention-boundary steady-state evidence |
| [IC-019](docs/issues/IC-019.md) | P1 | Open | Replica admission | Per-replica admission scope lacks a saturation acceptance gate |
| [IC-020](docs/issues/IC-020.md) | P1 | Open | Replica lifecycle | Cluster-wide admission and drain lifecycle lack acceptance |

## Working agreement

1. Select one issue, or one tightly coupled pair, from the highest-priority
   active group and set its frontmatter status to `in-progress`.
2. Confirm the live code still supports the finding, then add focused
   regression coverage for the original defect or missing contract.
3. Align implementation, current documentation, Lua examples, evaluations,
   and deployment guidance affected by the issue.
4. Run focused tests while iterating and the required all-target Rust gates
   before completion. Use live PostgreSQL only when the contract requires it.
5. Set an issue to `resolved` only after its acceptance criteria pass. Record
   the outcome, boundary, exact validation evidence, ISO completion date, and
   commit or PR when applicable.
6. Allocate the next never-reused ID and advance `docs/issues/HIGH_WATER_MARK`
   when adding a finding. Never lower the marker or delete a historical page.
7. Regenerate the indexes and run `bun run scripts/issues_registry.ts check`.

Historical audit baselines and cross-issue evidence are retained in
[`docs/issues/AUDIT_EVIDENCE.md`](docs/issues/AUDIT_EVIDENCE.md). Other plans
and product roadmaps are not engineering-status evidence.
