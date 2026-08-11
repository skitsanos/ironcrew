# IC-007 OpenShift v5 precursor canary

Status: superseded before closure. This is a sanitized, bounded OpenShift receipt for the completed v5 matrix. A later Railway run exposed a journal-write timeout defect, so v5 is retained as negative/precursor evidence and is not the artifact that can close IC-007. A homogeneous v6 rollout must repeat the source-binding matrix.

## Artifact and parity

The two application replicas ran the same digest-pinned v5 Linux artifact. Independent in-pod checks matched the binary, flow tree, and build-attestation hashes recorded in the JSON receipt. The shared Route returned 64/64 authenticated capability responses, split exactly 32/32 across pod UIDs `d3140ba3-00ee-4d1f-8d90-27beee1a02a4` and `308a7509-acc8-4164-9f0c-041992510ced`; header/body attribution and the advertised tuple agreed.

The exact assembly Dockerfile and build attestation are retained beside this report. `ic007-openshift-v5-build-manifest.json` contains the canonical manifest payload plus the repository text-file terminal LF; its canonical digest is computed over all bytes except that one LF. The source Dockerfile hash and the final assembly Dockerfile hash are deliberately distinct. The checked-in OpenShift manifest hash is also recorded separately because `deploy/` is outside the runtime build-input attestation.

## Matrix result

Cases 1-3 and 5-8 passed route attribution, replay/conflict, cancellation, encrypted cross-peer HITL, retained SSE, reconnect, and cursor edges. Case 10 passed per-pod run/conversation/SSE admission plus shared idempotency quota. Case 11 used an actual child-process `SIGKILL`, observed exit 137 without restart, preserved peer readiness, and reconciled once to `Abandoned`. The full staged key rotation failed closed on premature new-only, then passed expanded/old-active, mixed active keys, expanded/new-active, zero old references, and new-only. Case 13 passed drain fencing, EndpointSlice withdrawal, clean termination, stable replay, pod replacement, and a post-replacement cross-peer HITL run.

Cases 4 and 9 remain truthful nonportable boundaries. Unkeyed live run control returned `run_owned_by_another_instance` on the peer, and live conversation message/SSE remained owner-local while durable history was shared. No execution takeover or live-conversation portability is claimed.

## Security, networking, and configuration

Both application pods passed `restricted-v2` with arbitrary UID `1004800000`, non-root execution, dropped capabilities, no privilege escalation, RuntimeDefault seccomp, read-only root filesystems, bounded writable volumes, disabled service-account-token automount, and explicit requests/limits. Point-in-time resource samples are not a sustained ceiling or OOM proof.

The unauthenticated mock provider has no public Route or domain. Its Service is ClusterIP-only, and the data-boundary NetworkPolicy limits mock and PostgreSQL ingress to the exact canary app-pod selector. DNS egress covers UDP/TCP 53 and 5353 only to `openshift-dns`. Existing namespace policies are additive, so the canary policy is not presented as the sole ingress boundary.

The effective-config receipt retains all 90 approved manifest fields for every v5 phase. Secret values are absent; only presence booleans and safe SHA-256 key-material/keyring fingerprints remain. OpenShift deliberately used run-lease TTL 9 for this fault-injection canary, whereas the later Railway configuration used TTL 60. Cross-platform configuration equality is not claimed.

Bounded scans covered four pod logs and the exact canary database prefix: none of seven credential needles, HITL plaintext, or raw idempotency keys matched.

## Current state

Cleanup is intentionally deferred. At `2026-08-10T15:13:24Z`, the four labeled Deployments were ready, their four pods were Running with zero restarts, and all resources remained scoped by `ironcrew.io/test-run=ic007-final-20260810`. No cleanup mutation was made after v5 was superseded; these resources remain available for the v6 rollout.
