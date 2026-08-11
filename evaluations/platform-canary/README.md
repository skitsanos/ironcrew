# Platform canary attestations

These helpers create evidence inputs for Railway/OpenShift canaries; they do
not deploy, mutate a platform, or turn operator-provided `/capabilities` values
into runtime proof.

## Build receipt

`artifact_manifest.py` inventories the Docker build inputs used by the current
Dockerfile: `Cargo.toml`, `Cargo.lock`, `Dockerfile`, `.dockerignore`, and the
non-ignored regular files below `src/`, `examples/`, and `tests/`. Each entry
contains its sorted repository-relative path, byte size, and lowercase SHA-256.
The receipt also records the Git base commit, whether those inputs differ from
that commit, the supplied runtime-binary fingerprint, and a supplied flow-tree
fingerprint that the tool independently recalculates with the framed
`ironcrew-platform-flow-tree-v1` algorithm.

The tool rejects symlinks, special files, flow mismatches, and unsupported
`.dockerignore` rules rather than emitting incomplete evidence. It never walks
`.env`, `docs/`, or `target/`; pass an extracted/runtime binary outside those
repository paths. Ignored paths such as `examples/clients` and `.env` are not
opened or counted as dirty build inputs.

```sh
python3 evaluations/platform-canary/fingerprints.py flow /tmp/ironcrew-flows
python3 evaluations/platform-canary/artifact_manifest.py \
  --repository . \
  --binary /tmp/ironcrew-artifacts/ironcrew \
  --flow-root /tmp/ironcrew-flows \
  --flow-fingerprint sha256:<64-lowercase-hex>
```

Stdout is one compact, key-sorted UTF-8 JSON receipt. `manifest_sha256` is the
SHA-256 of the exact canonical JSON bytes in its `manifest` field (without a
trailing newline). Retain the exact binary through per-process verification.
If disposable-platform cleanup then removes every unpublished artifact copy,
the final receipt must say so: the digest proves the bytes observed during the
canary, while the source/build-input manifest is not a promise of a bit-for-bit
rebuild or a published artifact that can be downloaded later.

## Five attestation layers

- Source: base commit, input dirtiness, the complete input inventory, and its
  manifest SHA-256. A revision can bind both as
  `git:<commit>+manifest:<64-hex>`.
- Artifact: independently hash the exact binary staged for the image and the
  exact `/usr/local/bin/ironcrew` file in every running process.
- Flow: use `fingerprints.py flow`; it frames the domain, relative path, size,
  and content of every regular flow file.
- Config: use `fingerprints.py environment` only after the canary explicitly
  supplies every effective non-secret allowlisted value. Secret values are
  represented only by presence policy, never by their value or a guessable
  digest.
- Keyring: the same environment command binds sorted key IDs, active ID, and
  SHA-256-derived fingerprints of random 32-byte key material without emitting
  the keys.

The final IC-007 canary profile explicitly sets
`IRONCREW_EVENT_JOURNAL_WRITE_TIMEOUT_MS=5000` on every replica and binds that
effective value into the config fingerprint. This is a managed-PostgreSQL
canary override, not the application's 1500 ms default. It produces a 4-second
transaction-local PostgreSQL lock/statement timeout, a 15.15-second three-attempt
retry envelope, and a 15.65-second flush/terminal acknowledgement deadline. The
longer bound does not turn best-effort journal delivery into a completeness
guarantee; receipts must still prove numbered retained replay or record the
explicit incomplete fallback.

Map the resulting values to `IRONCREW_DEPLOYMENT_REVISION`,
`IRONCREW_ARTIFACT_FINGERPRINT`, `IRONCREW_FLOW_FINGERPRINT`,
`IRONCREW_CONFIG_FINGERPRINT`, and `IRONCREW_HITL_KEYRING_FINGERPRINT` as one
all-or-none tuple.

## Platform receipt boundary

IronCrew shape-checks and reports that tuple; it does not calculate it. A valid
platform receipt must inventory every active process, address it directly,
independently recalculate artifact/flow/config/keyring values in that process,
and compare them with authenticated `/capabilities`,
`X-IronCrew-Instance-Id`, and `process_start_id`. Record route distribution,
platform revision/pod identity, resources, lifecycle, and cleanup separately.
Equal advertised strings or repeated load-balancer samples are not parity
proof, and none of this proves conversation portability or execution takeover.

## Local runtime smoke

Static Lua validation does not execute conditional tasks or human-input
timeouts. Before building a platform image, run every canary flow through the
real HTTP server and bounded mock provider:

```sh
python3 evaluations/platform-canary/runtime_smoke.py \
  --ironcrew-bin target/debug/ironcrew \
  --flow-root evaluations/platform-canary/flows
```

The isolated SQLite probe must report four successful flows, four answered
questions, an exit code of zero, and exact provider counters `4/2/2/2`: one
counted effect in the replay fixture and one to materialize the unkeyed owner
before its local checkpoint. The repository Lua gate runs this smoke
automatically.
