# IC-007 Railway v5 precursor canary

Status: superseded before closure. The v5 TTL60 deployment reached two ready replicas and passed exact artifact/config/keyring attribution, routing, idempotent provider-effect, and encrypted shared HITL checks. It then failed the retained-SSE contract, so the remaining matrix stopped immediately and none of this receipt closes IC-007.

## Artifact and deployment

Railway promoted deployment `f49232b9-67b0-4a89-ac43-ef3e4591852f` in 114.979 seconds. Both active instances independently recomputed the same binary, flow-tree, effective-config, and HITL-keyring fingerprints. A 64-request public sample returned 64 successful capability responses, distributed 28/36 across the two instance IDs with exact header/body attribution.

The runtime artifact is the same digest-pinned v5 image used by the OpenShift precursor. Its exact assembly Dockerfile, canonical build manifest, and build-attestation JSON are retained once beside these reports under the `ic007-openshift-v5-*` filenames. The Railway-specific effective-config receipt retains every approved non-secret value plus secret-presence booleans and a keyring fingerprint; it retains no secret values.

## Functional result and blocker

Run `57b4f1c6-203a-4094-b526-796067d06715` completed `Success`, replayed cross-peer without a second external effect, rejected changed-body key reuse with 409, and kept the deterministic mock counts at two chat calls, one effect, one final response, and one tool-call response. This is bounded counted-effect evidence, not an exactly-once guarantee.

Run `a5dd61a7-e3d7-444c-a48b-66dcb9a97ae9` completed both encrypted cross-peer HITL questions. SQL byte and hash checks excluded the question and answer plaintext, duplicate answer returned 404, and the mailbox drained to zero. Malformed, cross-run, and ahead cursor requests returned 400, 400, and 409 with no-store.

Retained SSE failed for both runs. Every journal append exhausted three hard-coded 1,500 ms outer deadlines; the terminal reads therefore synthesized an unnumbered `run_complete` with `journal_complete: false`. Both authoritative run rows were `success`, but the scoped database contained zero journal events and zero journal-state rows. PostgreSQL separately logged four best-effort maintenance lock timeouts on the singleton event-usage row. The historical blocking PID was not logged and is not inferred as proven.

After the timeout storm, both processes remained live, but targeted readiness exceeded five seconds. A later public readiness request recovered to 200 in 8.105 seconds. Per-process pool samples moved from one to two checked-out connections out of a limit of two while PostgreSQL still showed four idle app backends and no current blocking PID. The receipt records this as transient client-side pool/readiness degradation, not as a persistent database lock.

## Current state

Cancellation, retained-cursor expiry, phase-specific admission/quota checks, exact-owner death/replacement, key rotation, IC-020 lifecycle, and final scans were deliberately not run after the mismatch. The three scoped services and public domain remain in place without an active volume so an authoritative v6 replacement can reuse the exact sandbox boundary. The project, production environment, and two pre-existing pending-deletion volume tombstones remain preserved.
