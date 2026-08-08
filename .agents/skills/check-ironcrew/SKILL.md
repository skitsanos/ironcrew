---
name: check-ironcrew
description: Run IronCrew's Rust, repository-policy, Lua-example, evaluation, security, PostgreSQL, replica, and release-build validation. Use when the user asks to check, validate, lint, test, verify, audit, or confirm that an IronCrew change is ready. Do not modify failures unless the user also asks for fixes.
---

# Check IronCrew

Run from the repository root. Treat `.github/workflows/ci.yml` as authoritative
if it diverges from this skill. Report drift during a validation-only request;
reconcile files only when the user also asks for changes.

## Select the gate

- Use focused tests while iterating, including negative, cancellation,
  concurrency, and boundary cases relevant to the change.
- Every Rust task finishes with `cargo fmt --all -- --check`,
  `cargo clippy --all-targets -- -D warnings`, and
  `cargo test --all-targets`.
- Add Lua validation when docs, examples, the Lua runtime, crew construction,
  tools, or public workflow behavior change.
- Add PostgreSQL validation when schemas, leases, idempotency, HITL, run events,
  reconciliation, or replica behavior change. Never use shared or production data.
- Use the complete gate before a requested commit/push, release preparation,
  branch integration, or when the user explicitly asks for every check.

## Repository policy

Run these first because they are cheap and fail before Rust compilation:

1. `bun run scripts/validate_skills.ts`
2. `bun run scripts/issues_registry.ts check`
3. `bun test scripts/tests/*.test.ts`
4. `actionlint .github/workflows/*.yml` when available
5. `bun run scripts/check_worktree.ts`
6. `git status --short`, followed by explicit review of every tracked and
   untracked source, policy, and documentation change

Use Bun's native YAML parser for repository-owned YAML checks. Do not add a
Python YAML dependency merely to validate skills or the issue ledger.

## Default Rust gate

Run in this order:

1. `cargo fmt --all -- --check`
2. `cargo clippy --all-targets -- -D warnings`
3. `cargo test --all-targets`
4. `cargo test --doc`
5. `cargo audit --deny warnings` after dependency or security-sensitive changes

Stop after a failure and preserve the actionable output. Do not add an allow,
ignore, or exception merely to make a gate green.

## Lua and evaluation gates

- Run `./scripts/check-lua-examples.sh` for broad Lua, docs, or example changes.
- Run `python3 -m unittest discover -s evaluations/crew-effectiveness -p 'test_*.py'`
  after evaluator changes.
- Run contract mode with the current debug binary and a disposable report path;
  contract mode validates orchestration and scoring, not crew superiority.
- Run `python3 -m unittest discover -s evaluations/replica-soak -p 'test_*.py'`
  after soak-harness changes.

Never expose `.env` values in commands, logs, reports, or summaries. Live
provider evaluation requires explicit intent and must record model, dataset,
repetitions, cost/token, latency, revision, and dirty-worktree boundaries.

## PostgreSQL and replica gate

Use a disposable PostgreSQL 15 database with a least-privilege test role and
set `IRONCREW_TEST_PG_URL` only for the test process. Run the CI integration
targets serially:

```bash
cargo test --locked --all-features \
  --test postgres_store_test \
  --test multi_replica_http_test \
  --test two_process_replica_acceptance_test \
  -- --test-threads=1
```

Run the short provider-free replica soak when replica lifecycle, routing,
leases, HITL, journals, or readiness changed. Record the database image/major,
process count, duration, workload, and whether evidence is local or deployed.
A skipped live test is not a pass.

## Report

Report every selected command as pass, fail, skipped, or not applicable.
Separate default Rust, PostgreSQL, process-level, soak, live-provider, Railway,
OpenShift, and platform-only CI evidence. “CI-equivalent” means every locally
reproducible gate; do not claim Windows or another unavailable runner was tested
locally. Do not claim a commit, push, release, or deployment that did not occur.
