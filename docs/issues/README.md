# IronCrew issue registry

This is the canonical registry for IronCrew engineering findings. Each finding
has a stable page whose frontmatter is the source of truth for status,
priority, area, and title. The registry is generated with
`bun run scripts/issues_registry.ts generate` and verified with
`bun run scripts/issues_registry.ts check`.

- Total findings: 20
- Active findings: 5
- Issued-through marker: [HIGH_WATER_MARK](./HIGH_WATER_MARK)
- Historical audit evidence: [AUDIT_EVIDENCE.md](./AUDIT_EVIDENCE.md)

| ID | Priority | Status | Area | Summary |
|---|---:|---|---|---|
| [IC-001](./IC-001.md) | P1 | Resolved | Replica safety | Owner death lacked a no-takeover replay acceptance gate |
| [IC-002](./IC-002.md) | P1 | Resolved | Replica availability | Replica maintenance database waits are not bounded |
| [IC-003](./IC-003.md) | P1 | Resolved | Run fencing | Local run fence can outlive the PostgreSQL lease deadline |
| [IC-004](./IC-004.md) | P1 | Resolved | Lease configuration | One-second run leases have no heartbeat safety margin |
| [IC-005](./IC-005.md) | P1 | Resolved | Replica cancellation | Owner death during durable cancellation lacks an acceptance gate |
| [IC-006](./IC-006.md) | P2 | Resolved | Replica diagnostics | Unkeyed wrong-owner control lacks a separate-process gate |
| [IC-007](./IC-007.md) | P1 | Resolved | Deployment validation | Railway and OpenShift replica routing lacks platform evidence |
| [IC-008](./IC-008.md) | P1 | Resolved | Distributed conversations | Live conversation control remains process-owned |
| [IC-009](./IC-009.md) | P1 | In progress | Product evidence | Crew effectiveness evidence is too small for product claims |
| [IC-010](./IC-010.md) | P1 | In progress | Observability | Execution and storage metrics are incomplete |
| [IC-011](./IC-011.md) | P2 | Open | Maintainability | Legacy oversized Rust modules lack a growth ratchet |
| [IC-012](./IC-012.md) | P2 | Resolved | Release governance | Accidental off-main release tags lacked a guard |
| [IC-013](./IC-013.md) | P1 | Resolved | Supply chain | Dependency audit allowed an unsound transitive warning |
| [IC-014](./IC-014.md) | P1 | Open | Release security | Release publication lacks a platform-enforced trusted control plane |
| [IC-015](./IC-015.md) | P1 | Resolved | Release automation | Release automation admitted ambiguous tags and non-cascading Docker triggers |
| [IC-016](./IC-016.md) | P1 | Resolved | Replica encryption | Staged HITL key rotation lacks a replica acceptance gate |
| [IC-017](./IC-017.md) | P2 | Resolved | Replica event replay | Durable SSE edge cases lack a separate-process gate |
| [IC-018](./IC-018.md) | P2 | Open | Replica capacity | Replica soak lacks retention-boundary steady-state evidence |
| [IC-019](./IC-019.md) | P1 | Resolved | Replica admission | Per-replica admission scope lacks a saturation acceptance gate |
| [IC-020](./IC-020.md) | P1 | Resolved | Replica lifecycle | Cluster-wide admission and drain lifecycle lack acceptance |
