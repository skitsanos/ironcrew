# IronCrew issue registry

This is the canonical registry for IronCrew engineering findings. Each finding
has a stable page whose frontmatter is the source of truth for status,
priority, area, and title. The registry is generated with
`bun run scripts/issues_registry.ts generate` and verified with
`bun run scripts/issues_registry.ts check`.

- Total findings: 39
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
| [IC-009](./IC-009.md) | P1 | Resolved | Product evidence | Crew effectiveness evidence is too small for product claims |
| [IC-010](./IC-010.md) | P1 | Resolved | Observability | Execution and storage metrics are incomplete |
| [IC-011](./IC-011.md) | P2 | Resolved | Maintainability | Legacy oversized Rust modules lack a growth ratchet |
| [IC-012](./IC-012.md) | P2 | Resolved | Release governance | Accidental off-main release tags lacked a guard |
| [IC-013](./IC-013.md) | P1 | Resolved | Supply chain | Dependency audit allowed an unsound transitive warning |
| [IC-014](./IC-014.md) | P1 | Resolved | Release security | Release publication lacks a platform-enforced trusted control plane |
| [IC-015](./IC-015.md) | P1 | Resolved | Release automation | Release automation admitted ambiguous tags and non-cascading Docker triggers |
| [IC-016](./IC-016.md) | P1 | Resolved | Replica encryption | Staged HITL key rotation lacks a replica acceptance gate |
| [IC-017](./IC-017.md) | P2 | Resolved | Replica event replay | Durable SSE edge cases lack a separate-process gate |
| [IC-018](./IC-018.md) | P2 | Resolved | Replica capacity | Replica soak lacks retention-boundary steady-state evidence |
| [IC-019](./IC-019.md) | P1 | Resolved | Replica admission | Per-replica admission scope lacks a saturation acceptance gate |
| [IC-020](./IC-020.md) | P1 | Resolved | Replica lifecycle | Cluster-wide admission and drain lifecycle lack acceptance |
| [IC-021](./IC-021.md) | P1 | Resolved | Sandbox integrity | Hook and condition Lua evaluation escapes the crew sandbox |
| [IC-022](./IC-022.md) | P1 | Resolved | Secret handling | Shell tool leaks the process environment to model-controlled commands |
| [IC-023](./IC-023.md) | P1 | Resolved | Runtime limits | Default memory value cap rejects valid task output and fails the run |
| [IC-024](./IC-024.md) | P1 | Resolved | Documentation accuracy | Tool-sandbox documentation misstates http availability |
| [IC-025](./IC-025.md) | P2 | Resolved | Provider fidelity | Structured output is silently dropped by Anthropic and Responses providers |
| [IC-026](./IC-026.md) | P2 | Resolved | Provider fidelity | Image attachments are silently dropped by the Responses provider |
| [IC-027](./IC-027.md) | P2 | Resolved | Egress security | Secret-bearing custom headers survive cross-host redirects |
| [IC-028](./IC-028.md) | P2 | Resolved | Configuration safety | Unknown Lua configuration keys are silently ignored |
| [IC-029](./IC-029.md) | P2 | Open | Async discipline | Blocking filesystem and parse work runs on Tokio workers in several paths |
| [IC-030](./IC-030.md) | P2 | Resolved | Orchestration correctness | Foreach total failure does not gate dependent tasks |
| [IC-031](./IC-031.md) | P2 | Resolved | API contract | Store failures are reported as 404 on run read and delete |
| [IC-032](./IC-032.md) | P2 | Resolved | Release governance | Taskfile publish can overwrite signed release images |
| [IC-033](./IC-033.md) | P2 | Open | Provider robustness | Provider request timeout is a fixed 120s total deadline |
| [IC-034](./IC-034.md) | P3 | Resolved | Error hygiene | Error responses and provider error paths leak internals or lose status |
| [IC-035](./IC-035.md) | P3 | Open | Failure visibility | Silent degradation on malformed tool arguments and failed hooks |
| [IC-036](./IC-036.md) | P3 | Open | API hardening | Residual HTTP hardening gaps in proxy trust, timeouts, CORS, and audit coverage |
| [IC-037](./IC-037.md) | P3 | Open | Provider maintainability | Provider scaffolding is triplicated and RateLimiter::new can panic |
| [IC-038](./IC-038.md) | P3 | Resolved | Documentation accuracy | Documentation and example drift across nodes, endpoints, and model pins |
| [IC-039](./IC-039.md) | P3 | Resolved | Build hygiene | Build context, dev-dependency comment, and client material housekeeping |
