# IronCrew engineering issues

The canonical engineering ledger is maintained in
[`docs/issues/README.md`](docs/issues/README.md). Individual findings use
stable paths such as [`docs/issues/IC-001.md`](docs/issues/IC-001.md).

## Active findings

| ID | Priority | Status | Area | Summary |
|---|---:|---|---|---|
| [IC-029](docs/issues/IC-029.md) | P2 | Open | Async discipline | Blocking filesystem and parse work runs on Tokio workers in several paths |
| [IC-033](docs/issues/IC-033.md) | P2 | Open | Provider robustness | Provider request timeout is a fixed 120s total deadline |
| [IC-035](docs/issues/IC-035.md) | P3 | Open | Failure visibility | Silent degradation on malformed tool arguments and failed hooks |
| [IC-036](docs/issues/IC-036.md) | P3 | Open | API hardening | Residual HTTP hardening gaps in proxy trust, timeouts, CORS, and audit coverage |
| [IC-037](docs/issues/IC-037.md) | P3 | Open | Provider maintainability | Provider scaffolding is triplicated and RateLimiter::new can panic |

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
