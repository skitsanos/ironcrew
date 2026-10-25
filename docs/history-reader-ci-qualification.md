# History reader CI candidate qualification

The history reader is an unpublished candidate under [IC-055](issues/IC-055.md), not a release or
deployment. The original [isolated-restore receipt](legacy-history-reader-acceptance.md) remains
historical evidence for the previous local source and executable; later candidates require new gates.

CI's `Release Build` checks out the exact pull-request head (or push commit) and builds with the
locked dependency graph. `scripts/history_reader_artifact.py` packages the native Linux AMD64 ELF
into a deterministic archive and records source commit, repository, workflow run/attempt, target,
archive hash and executable hash. GitHub retains the candidate under an immutable artifact ID for
14 days. This bounded CI retention is not a permanent release or an offsite data backup.
The build/package boundary requires a clean checkout, and all artifact files have regular-file and
byte limits checked before bounded reads. Verification extracts the same archive bytes whose hash
was checked, rather than reopening a mutable file after hashing it. The expanded tar stream is capped
at 256 MiB plus 20 KiB overhead; only one fixed USTAR regular-file header is accepted. GNU/PAX extended
metadata is rejected without following attacker-declared lengths.

`History reader artifact` jobs on PostgreSQL17 and latest download the producer's exact artifact ID,
bind the receipt digest and all source/run identity fields, verify archive/executable hashes before
extracting a single regular executable, and run that downloaded executable. Their database contains
only authored synthetic history. A SELECT-only role exercises unknown/zero/unavailable usage,
arbitrary-precision legacy counters, invalid present receipts, scope/pagination and dotenv boundaries.
The complete authored rows and table schema must remain unchanged. No retained private history or
production configuration enters GitHub Actions. No provider, release or deployment action is invoked.

For an independent download, select the exact Actions run and artifact ID; record the GitHub artifact
ZIP digest from its API metadata and verify the downloaded ZIP before unpacking. Then run the verifier
with the independently selected commit/repository/run/attempt and the producer's receipt SHA-256.
Do not trust an archive's self-declared identity, a mutable artifact name, the current branch head,
or the unchanged package version alone. Retain these aggregate receipts and the verified archive
locally before CI expiry. Qualify a later commit again, even if its change is only documentation.

The normal runtime/HTTP decoder remains strict. Read-only operator inspection does not establish
replay, resume or an engine downgrade. Production use, immutable release publication and coordinated
App/Crew deployment remain separate authorization and acceptance boundaries.
