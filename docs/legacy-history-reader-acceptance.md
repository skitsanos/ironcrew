# Read-only legacy history reader — acceptance, 2026-10-24

Scope: [IC-055](issues/IC-055.md). Local uncommitted candidate based on develop
`64461e6fe75b8c1438e061c60146b4445639be3e`, not a release or deployment. Unrelated existing
`docs/issues/IC-052.md` changes are preserved. No production connection or provider call is used.

## Result and contract

`ironcrew history list/inspect` provides an operator-only PostgreSQL history path using explicit
process configuration, a SELECT-only role and read-only transactions. It does not instantiate a
runtime store, load `.env` or flow files, initialize schema, reconcile/prune records, append audits,
or execute/resume any work. Connection errors and stored-value validation failures are sanitized.
See [CLI syntax, limits and output](cli.md#history).

The envelope contains the original row as raw JSON and a separate optional checked-usage projection.
Missing checked task/run receipts are null; existing unavailable snapshots stay unavailable. Neither
legacy counters nor missing fields become known zero. Arbitrary-precision legacy numbers retain
their exact spelling. Present malformed checked receipts fail rather than falling back to counters.
Runtime `RunRecord`/`TaskResult`, HTTP, accounting, replay and resume remain strict and unchanged.

This tool covers PostgreSQL 17+ run rows in the configured public-schema prefix. It does not add
JSON/SQLite archive support, session/HITL recovery, a new HTTP endpoint or an engine downgrade path.
The CLI requires the postgres feature and explicit `DATABASE_URL`; it never loads ancestor dotenv
configuration. The ordinary `ironcrew inspect` command retains its existing runtime-store behavior.

## Actual isolated restore

The authenticated native PostgreSQL archive from the TranscriptIntel backup rehearsal
(workspace receipt `docs/backup-restore-rehearsal-2026-10-24.md`, outside this repository)
was restored with native 18.6 tools into a new owned tmpfs database. Original ciphertext remains
unchanged. Only PostgreSQL was decrypted for this phase; no production configuration was loaded.
The archive and separate recovery key remain in their protected original locations.

An authored SELECT-only role, with session read-only default, reads `public.ti_runs`. The actual
candidate CLI, without a server, lists all original IDs exactly once across three pages and inspects
all records without creating `.ironcrew` state or requiring historical flow directories.

| Acceptance | Observed result |
|---|---|
| Original current-prefix runs | **137/137** listed and inspected; **137/137** original row objects match |
| Previously blocked legacy runs | **80/80** readable through this separate history view |
| Legacy task results | **124** retained, all **124 checked task usages remain null** |
| Checked run snapshots | **137** preserved exactly: 55 complete, 82 unavailable |
| Populated checked task results | Zero in this production snapshot; synthetic regressions cover current and mixed task receipts |
| Empty flow scope | All **64** retained; exact empty-scope filtering works without invented routing |
| Negative reads | Wrong flow, missing run and invalid prefix fail closed |
| Entire restored database | Before/after normalized native `pg_dump` hashes identical; all 19 tables/8,371 rows and schema retained |

The matching complete-database fingerprint is
`51307a9f8b335b90381203e18e30ff96bc79401637a8275d860b1408dac92445`.
Dump comparison omits owner/ACL output and normalizes randomized `\restrict` tokens identically on
both sides; no startup or history-row repair is performed. Synthetic integration fixtures use a
different database, never the restored private history database.

The retained backup's ciphertext SHA-256 is
`b5d62925eeccc269ea1ed75d07dd7b22a70fcca0c783253dd379714ab602451e`.
All original recovery archives are authenticated before extraction. Private run/task content is
processed in memory and never emitted into safe evidence, issue pages, tool output or providers.

## Regression and gate evidence

Ten focused tests cover legacy/current/mixed receipts, known zero versus unknown, exact unsigned
counts, arbitrary-precision integers/decimals, `1e999`, original raw serialization, SQL/public status
spellings, malformed/null/contradictory usage, invalid headers, byte/task bounds and ancestor-dotenv
isolation. The PostgreSQL fixture also checks empty/retired scopes, pagination, invalid identifiers,
SQL-side oversized row/summary rejection and unchanged rows.

Independent read-only review found and corrected two implementation gaps before final acceptance:
ordinary JSON `Value` rounded legacy numbers, and the first integration fixture required CREATEROLE.
Raw JSON storage now preserves those numbers; the fixture runs with CI's existing restricted account.
The separate restore setup supplies the SELECT-only role without weakening CI privileges.

Final source/test manifest SHA-256:
`53898503f7d49bd88627e133bdba7fdd6efde127b41605b43efb79aade8154a4`.
It hashes Cargo manifests and all Rust source/tests. No source changes overlap the final gates.

- `cargo fmt --all -- --check`: pass.
- `cargo clippy --all-targets -- -D warnings`: pass.
- `cargo test --all-targets`: **1,570 pass, three intentional ignores** across 70 executables.
  Database-enabled tests are qualified separately, not inferred from their no-URL default returns.
- `cargo test --doc`: pass (zero runnable doctests).
- `cargo audit --deny warnings`: pass; 415 locked crates, fresh RustSec advisory database.
- PostgreSQL floor/latest serial gate: **78 pass on 17.11 and 78 pass on 18.6**, zero failures/ignores.
  All five suites run serially on each major with restricted CI roles, including actual reader,
  usage/store, HTTP and genuine multi-process tests. Only the authored integration databases are targeted.
- Lua: 67 declaration/syntax checks, five construction evaluations and five offline probes pass.
- Python repository policy: 80 pass; Bun repository policy: 88 pass / 755 assertions.
- Module size: pass, 41/41 unchanged legacy exceptions; new modules stay below 300 lines.
- Skills and workflow lint pass. Issue registry, documentation links and final worktree checks
  pass; their final closure receipts are retained alongside the gate logs.

The database images were pulled immediately before testing: floor **17.11**, digest
`sha256:b8faeff7712663c2773a190181e1246e37e364b887611c04658c28b44e2408fe`, and latest **18.6**, digest
`sha256:edb0c08dc33befd0262ab3b5cf1c4f3f8cfd8b09e88c05b45adee29d6761ddb6`.
Both integration roles match hosted policy: NOSUPERUSER, NOCREATEDB, NOCREATEROLE, NOINHERIT and
NOREPLICATION. These are local Docker/macOS checks, not native Linux, Windows or hosted CI evidence.
No commit, push, publication, service restart or deployment is performed.

## Retention and remaining boundaries

Aggregate evidence is ignored under `output/legacy-reader-2026-10-24-9680e55d/`. It includes exact
source and executable identity, native restore/reader results, before/after fingerprints, restricted
roles and gate logs. The candidate is a local build of unchanged package version 4.0.4; its source
manifest and executable hash identify this unpublished change, not the version string alone.

The two owned containers and their anonymous test volume are removed, and their disappearance is
verified. The decrypted restore/configuration/private diagnostics and **4,329,954,855 bytes** of owned
build cache are removed. Original encrypted archives authenticate and retain identical hashes; the
separate recovery key remains protected. Safe receipts and one 96,517,088-byte executable remain at
`output/legacy-reader-2026-10-24-9680e55d/candidate/ironcrew`, SHA-256
`b2572a9c4a0d46956c55c4e24af9e7baee8bef57dd311347b069000bdc0d65fb`.
It does not replace an installed binary or running service. All six owned temporary helper/context
files are removed and their absence verified after final checks. Cleanup and independent read-only
acceptance are recorded in the retained receipts.

The prior normal-runtime decoder limitation remains intentional. Qualifying this read-only operator
path does not make the 80 legacy records executable or readable through the old HTTP endpoint.
Publish/CI-qualify an immutable release containing this reader before treating it as an available
production recovery tool. Establish an approved offsite archive copy independently of local reader
acceptance. Deployment and coordinated cutover authority remain absent.
