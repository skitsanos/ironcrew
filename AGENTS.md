# IronCrew repository guidance

## Scope and sources of truth

- IronCrew is a Rust runtime for Lua-defined AI agent crews. Treat `src/`,
  `tests/`, `docs/`, `examples/`, evaluations, and the public CLI/HTTP contracts
  as one product surface.
- Use `docs/issues/README.md` as the canonical engineering registry and
  `docs/issues/IC-NNN.md` as the stable finding record. Root `ISSUES.md` is a
  generated active-only index. Other plans or roadmaps do not override issue
  lifecycle evidence.
- Issue IDs are permanent and never reused. When adding a finding, allocate the
  next ID after `docs/issues/HIGH_WATER_MARK`, advance that marker in the same
  change, and never lower it or delete a historical issue page. The registry
  compares the marker with `HEAD` locally and with the trusted PR/push base in
  CI, so a coordinated marker rollback and tail-page deletion must fail.
- Preserve user-owned and unrelated worktree changes. Inspect `git status` and
  overlapping diffs before editing.
- Work in English. Do not commit, push, merge, tag, publish, deploy, or mutate a
  remote service unless the user explicitly asks.
- Keep `docs/superpowers/` untouched unless the user explicitly brings those
  agent-authored plans into scope.

## Rust and runtime design

- Keep new production modules focused and normally at or below 300 lines. Do
  not create a new module above 400 lines. Existing oversized modules are
  legacy debt: avoid growing them and extract cohesive responsibilities when
  making related changes.
- Prefer typed errors and state transitions, explicit ownership, bounded work,
  cancellation-safe cleanup, and atomic persistence over implicit side effects.
- Keep Tokio workers free of blocking filesystem, process, parsing, or database
  work. Retain admission permits until physical work has stopped.
- Treat Lua, paths, URLs, provider/tool responses, storage rows, request bodies,
  and environment configuration as untrusted input. Enforce byte, count,
  concurrency, and time limits before materialization.
- Keep secrets out of errors, logs, fixtures, reports, and command output.
  `.env` may be consumed by tests, but never print or commit its values.
- Avoid new dependencies when the standard library or an existing dependency
  is sufficient. Do not weaken the dependency audit without a documented review.

## Multi-replica and deployment boundaries

- PostgreSQL 15+ is IronCrew's shared durable coordination layer; it is not a
  distributed Lua execution engine. Preserve run ownership, lease fencing,
  idempotency, HITL mailbox encryption, and terminal-write compare-and-set rules.
- Distinguish process-local execution, conversation handles, admission, and live
  SSE from shared PostgreSQL records and journals. Never claim execution
  failover, exactly-once external effects, or platform routing without the
  matching acceptance evidence.
- For Railway and OpenShift work, analyze limits per pod and in aggregate as
  `replicas × per-replica limits`. Keep readiness, graceful drain, arbitrary
  container UID, injected port, and PostgreSQL pool behavior in scope.
- Storage integration tests are destructive. Use disposable, explicitly named
  PostgreSQL databases or containers and remove only resources created by the
  test. Never point a test at shared or production infrastructure.
- Docker-backed PostgreSQL tests use the newest patched image of IronCrew's
  minimum supported major. Pull the moving `postgres:15` tag immediately before
  a local run, reuse that tag across test suites, and keep CI on `postgres:15`.
  Do not substitute `postgres:latest`, which can silently change the database
  major, or create dated/per-test image tags that accumulate locally.
- Start disposable test containers with an explicit name and `--rm`; stop them
  after the gate and verify they disappeared. Inspect Docker ownership before
  cleanup. Never run a global Docker system, image, builder, container, or volume
  prune as part of a gate. Remove only resources positively attributed to the
  current IronCrew test, and never remove resources owned by another project.

## Documentation, Lua examples, and evaluations

- When behavior changes, update the nearest CLI/API/storage/architecture docs,
  relevant README sections, Lua examples, and deployment guidance in the same
  change.
- Keep names, parameters, defaults, environment variables, status codes,
  limits, ownership scopes, and failure behavior consistent across code and docs.
- Static `ironcrew validate` is not runtime proof. Use graph capture or a
  representative execution probe when Lua construction, tools, HITL, provider
  behavior, or orchestration semantics matter.
- Treat crew-effectiveness contract mode as harness validation only. Separate
  mock-provider, live-provider, local-process, soak, and deployed evidence, and
  record dataset/model/repetition/cost boundaries.
- After issue metadata changes, run `bun run scripts/issues_registry.ts generate`
  and then `bun run scripts/issues_registry.ts check`.

## Validation

- For Rust changes, finish with:
  1. `cargo fmt --all -- --check`
  2. `cargo clippy --all-targets -- -D warnings`
  3. `cargo test --all-targets`
- Add focused negative, cancellation, concurrency, and boundary tests while
  iterating. Run the live PostgreSQL targets with `IRONCREW_TEST_PG_URL` when
  storage, schemas, leases, idempotency, HITL, journals, or replicas change; a
  skipped integration test is not evidence.
- Run `./scripts/check-lua-examples.sh` after Lua runtime, public workflow, docs,
  or example changes. Run the affected Python evaluation unit/contract checks
  after evaluation changes.
- Run `bun run scripts/validate_skills.ts` after changing repository skills,
  `bun test scripts/tests/*.test.ts` after repository-policy changes, and
  `actionlint .github/workflows/*.yml` after workflow changes when available.
- Run `cargo test --doc` after public Rust API documentation changes and
  `cargo audit --deny warnings` after dependency or security-sensitive changes.
- Before a requested commit or push, run every locally reproducible CI gate for
  the affected surfaces. Run `bun run scripts/check_worktree.ts`, inspect
  `git status --short`, and review the complete tracked and untracked diff.
  Report platform-only jobs such as Windows as CI evidence rather than claiming
  they ran locally.

## Issue lifecycle and completion

- Confirm a ledger finding against current code before implementation. Treat
  the issue page as a testable hypothesis, not proof that the defect still exists.
- Allocate independent findings monotonically through `HIGH_WATER_MARK`; do not
  fill an apparent gap, recycle a resolved ID, or remove prior evidence.
- Set an issue to `resolved` only after implementation, regressions,
  documentation/examples, contract boundaries, and exact validation evidence
  are recorded. Keep historical evidence honest when later behavior changes.
- Distinguish focused, default, PostgreSQL, process-level, soak, Railway, and
  OpenShift validation. Do not present one level as proof of another.
- After completing a goal or development phase, propose at most three
  prioritized, bounded next goals.
