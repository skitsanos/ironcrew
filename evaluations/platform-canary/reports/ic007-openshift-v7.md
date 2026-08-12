# IC-007 OpenShift v7 canary receipt

Recorded 2026-08-10 against the temporary `gedankrayze-dev` namespace and the exact selector `ironcrew.io/test-run=ic007-final-20260810`.

## Outcome

The final v7 artifact passed the applicable OpenShift IC-007 matrix, staged HITL key rotation, real owner `SIGKILL`, and the IC-020 explicit-drain/replacement lifecycle. Cases 4 and 9 remain intentionally process-local boundaries; this receipt does not claim Lua execution takeover, portable live conversations, or exactly-once external effects.

The canary is not described as security-clean. Docker Scout found five current unfixed HIGH/CRITICAL operating-system CVEs, and the shared sandbox's additive `allow-same-namespace` policy prevents an exclusive mock-ingress claim. Both limitations are retained below without suppression.

Cleanup is complete. Both exact canary selectors are empty, all attributable
database tables are gone, the namespace baseline and zero quota usage were
restored, and no broad deletion or prune was used.

## Exact artifact and configuration

- Tag: `ironcrew:ic007-platform-final-v7-20260810`
- OCI index: `sha256:bd1c8a1df0f98e2c8d92ee59408c44a98a882848dd33e5a081d973da01ae8f7f`
- Linux/amd64 manifest: `sha256:725bf9b4cb3e91bbb3c4f6a58bc14987be18b143816611318037c257bc6d6479`
- Binary: `sha256:b80ec1f50ad5f9842b9b826979bc7f1e984da1fe692730cdf7e9e1bd8a9467e0`
- Flow tree: `sha256:7a66cd31b503994bdf2338c6a599d0e1762a96bfaebad47d295d10e267fa4256`
- Canonical build manifest: `sha256:031a037a2fec088ddb3a07ff8f9467734513a3ef2c52a6efe91ab119b5ad6cd5`
- Assembly context manifest: `sha256:4d04389f7c8d013c419e2cf5bbe65189f1d1e453bd97bea828670154aee74ee6`
- Revision: `develop-c4799a3c3b8a2441243ad512436d1cb649275cf4+manifest-031a037a2fec088ddb3a07ff8f9467734513a3ef2c52a6efe91ab119b5ad6cd5`
- Original full-matrix baseline config: `sha256:65d63e3ff35f7268f0b9365d8fbb39e3442ee7f3499b9a7e1277889e32391fc8`
- Original full-matrix final new-only keyring: `sha256:41989ab25945921fe33de6f7d21d7f8000e4fb5b9eefe730757d981e0975d01f`
- Authoritative rotation-rerun config: `sha256:9aa9d058ddb51fe6def22a83a60fd5d689f1d9294a6c3d1146842facfd816647`
- Authoritative rotation-rerun final new-only keyring: `sha256:71cd52a2cca38da79108143608d1d17fd7d6aac36295f9994441f91eba935031`
- Effective contract: 107 required values, one optional value, five secret-presence fields, 113 fields total; TTL 60, journal-write timeout 5000 ms, optional SSE output bound absent.

The authoritative rotation-parity rerun used fresh random keys, prefix
`ic007r_`, provider-free URL placeholder `http://provider.invalid/v1`, and
the separately labeled config and final keyring above.
It retained 14 complete process attestations across seven stable phases in
`ic007-openshift-v7-rotation.json` (`sha256:a5555716594f981a03caa6362daaa56efb05b5e6cd80cf2181236784bdd1f433`).

The final `deploy/openshift.yaml` source hash is `sha256:eb1fb4f0226322be128cea9f21481103b7cc81e798fdc78f5057afe86a7f850b`. Its final change corrected a comment only; the runtime spec and artifact tuple did not change and required no rollout.

OpenShift and Railway deliberately have different config fingerprints even though both final profiles use TTL 60, journal-write timeout 5000 ms, and an absent optional SSE bound. Prefixes, provider hostnames, secret-presence profiles, platform identity, and routing differ. No cross-platform config-equality claim is made.

The exact assembly Dockerfile bytes, build attestation, build manifest, and clean ten-file assembly-context inventory are retained as the canonical `ic007-v7-*` assets. All three verifier helpers were hashed independently inside every final app pod before their output was trusted.

## Platform matrix

| Case | OpenShift v7 evidence | Result |
| --- | --- | --- |
| 1 / 12 parity and routing | 64 shared-route capability requests mapped exactly to both pod UIDs, 33 / 31, with the expected artifact/config/flow/keyring/revision tuple and receiver header. | Pass |
| 2 / 3 replay, conflict, effect | Run `c599b631-f2b7-4451-b98b-4d82f5c20f46`; replay returned 200 and `Idempotency-Replayed: true`; conflicting body returned 409; mock stayed exactly 2 chat / 1 effect / 1 final / 1 tool-call response. | Pass |
| 4 unkeyed owner boundary | Run `f103f2b7-75f2-42eb-b682-c63ae9da5db0`; peer could read durable state but question and abort returned 409 `run_owned_by_another_instance`; owner aborted without a duplicate effect. | Intentional boundary |
| 5 peer cancellation | Run `e6bb255d-407d-415f-886b-9a5e4c7aaef7`; peer queued the first cancellation request and the run became Aborted. | Pass |
| 6 cancellation race | Run `94f981eb-f728-4edf-abad-e0dc9991e573`; owner abort and peer request converged to one terminal event, zero mailbox rows, and 404 for post-terminal control. | Pass |
| 7 keyed HITL and SSE | Run `a132b732-3f9d-4390-a2c6-ad6e56d1dae9`; encrypted pending row, two peer answers, duplicate 404, Success, event IDs 1-7, reconnect after 2 returned 3-7. | Pass |
| 8 cursor edges | Run `d184a8be-5ec7-4aee-80f4-dbf66cebc014`; expired/malformed/cross-run/ahead returned 409/400/400/409 with exact body classes immediately and 5.33 s later. No transient mismatch occurred. | Pass |
| 9 conversation boundary | Conversation `ic007-v7-1faab85bb1c2485d9046`; durable history was peer-readable, but peer live message/events were 404. | Intentional boundary |
| 10 local limits | One run, conversation, and SSE stream per pod; second work returned 503 and second SSE returned 429. Aggregate capacity was exactly two of each. | Pass |
| 10 shared quota | Run `5d2a7844-4969-4c64-baeb-3828d9bde688`; prefix `ic007q_`, one record/principal/in-flight; both peers returned 429, one 64-byte hash existed, no raw key existed, then all ten temporary tables were dropped. | Pass |
| 11 owner loss | Run `34d06997-b334-428d-91e9-69e96d00f06f`; real child `SIGKILL`, exit 137, peer remained ready, lease reconciliation produced Abandoned, stable replay, and no duplicate effect. Replacement UID `67b96350-ea67-497c-8d79-ac32dc9746f3` then completed run `81262066-267c-4e06-9f36-7579643c88f6`. | Pass |
| 13 explicit drain and replacement | Run `c7543a92-7fde-40f9-8b9c-455f515beaac`; details below. | Pass |

The cursor terminal poll occurred at `2026-08-10T16:20:51.705645Z`; the journal tail was sequence 7 at `2026-08-10T16:20:51.471697Z`, and the first cursor probe began 10 microseconds after the terminal poll. The retained range was 4-7. A no-cursor replay emitted `journal_gap` at ID 3 followed by 4-7.

No non-test pods, Deployments, StatefulSets, DaemonSets, Jobs, or CronJobs were present during the counted provider phases. Provider deltas are paired with that inventory; they are not justified by a sole-ingress claim.

## Staged key rotation

The premature new-only pod exited 1 with the fixed unavailable-key classification. Its logs matched none of the seven exact credential needles, and the retained old ciphertext row was byte/hash-identical before and after startup.

Run `4a094b44-94c5-4107-bdf1-7c9f8ccd5063` retained an old-key row with a 12-byte nonce, 235-byte ciphertext, and ciphertext hash `43a12f7f35cf5d76f1473d990c43acab1a3cef6c9925cdd834eca86dbb3001e3`. The sequence then passed expanded-old, mixed expanded-old/expanded-new, expanded-new, zero old references, and new-only.

Mixed-active run `1c6688ef-66ff-4d5e-9ac7-8396cc3055b2` retained a new-key row with a 12-byte nonce, 235-byte ciphertext, and ciphertext hash `6442570dd32a5541470be73f236a561b91e863b74dc0aea30ed2b623145be201`. Final new-only cross-peer run `cde9d6a6-8f69-4776-afb5-cc1dd2a2640a` succeeded.

Every rotation pod recomputed the 113-field config and keyring marker with the attested helper. Rotation temporarily used the separately recorded Lua-300 manifest; TTL remained 60 and the journal-write timeout remained 5000 ms.

An authoritative scoped rerun closed the process-inventory retention gap. Its
old/new/final run IDs were respectively
`a56a301e-6c3e-47b0-93d6-e70b74e5b4e9`,
`36358330-b6b2-4dda-bfdd-f3d51c1e11a9`, and
`53abb0b5-ec13-4804-9bdf-034b470f7401`. The old ciphertext hash
`4a55d47f0001ed3df44da0ba0a2e99e95c97d6ba4c910e2372fbe471f26adada`
was byte-identical across the negative startup; the new-active ciphertext hash
was `dc3ac3030e76fe100f5a23800a345eddcbc7b4e26c25de61d82e3870a0e62b12`.
There were zero old references before retirement and the final run succeeded.

For every Ready, non-deleting process in all seven stable phases, the retained
receipt binds Deployment identity/generation, pod UID, restart count, direct
Route, receiver header, instance/process-start IDs, exact OCI index and Linux
manifest, advertised revision/fingerprints, and independent binary, flow,
config, keyring, build-attestation, and three-helper hashes. The failing
premature new-only pod exited 1 and its log matched no credential needle.

## Explicit drain and replacement

For run `c7543a92-7fde-40f9-8b9c-455f515beaac`, `SIGUSR1` dropped direct readiness in 2.416 s and removed the exact pod UID from EndpointSlice readiness in 3.237 s. The durable snapshot changed only from:

`waiting_for_input | running | owner_draining=false | cancel=false | mailbox=1 | events=1`

to:

`waiting_for_input | running | owner_draining=true | cancel=false | mailbox=1 | events=1`.

Three direct mutations returned non-cacheable 503 `instance_draining` with `Retry-After: 1`. Peer abort and answer returned non-cacheable 503 `run_owner_draining` with the same retry header. Direct run/question reads, one retained SSE frame, and lifecycle metrics remained observable.

Real PID 1 `SIGTERM` exited 0. SQL converged to `aborted | one run_complete | zero mailbox | completed ledger | owner_draining=true | cancel=false | response 200`; same-key replay through B retained the original run and owner.

The distinct replacement UID `c61b252f-3005-4a54-a4d9-2559c50297ac` independently passed tuple/helper/config verification. During replacement the affinity-free shared Route passed 60/60 readiness, 60/60 liveness, and 60/60 capabilities with zero failures and a 32 / 28 receiver split. Replacement run `b1552ed9-1b99-400c-a5a7-355049cf9445` completed Success after two peer answers.

Immediate mailbox and journal reads are not asserted to be one atomic view: a precursor probe observed the mailbox before the first journal append. The accepted run retained each distinct bounded SQL transition and waited for journal convergence before signalling; terminal-row and `run_complete` convergence were handled the same way.

## Security, resources, and network boundary

Both final app pods ran under `restricted-v2` as UID `1004800000`, GID `0`, RuntimeDefault seccomp, read-only root filesystem, no privilege escalation, all capabilities dropped, and no mounted service-account token. Writes to `/etc` failed; an exact `/tmp` probe succeeded and was removed. Each pod requested 100m CPU / 128 MiB memory / 64 MiB ephemeral storage and was limited to 1 CPU / 512 MiB memory / 512 MiB ephemeral storage.

`oc adm top` was unavailable. `/proc` and protected metrics instead showed 17,956,864 and 15,466,496 RSS bytes, 33,423,360 aggregate; two open PostgreSQL pool connections per pod and four idle sessions total; zero active runs, SSE connections, EventBus instances, retained events, and retained bytes.

The mock provider had no Route, external IP, load balancer, or node port; it was a ClusterIP service. The canary data policy explicitly selected mock/PostgreSQL and allowed ingress from only labeled canary app pods. The app egress policy allowed DNS UDP/TCP 53 and 5353 only to `openshift-dns`, PostgreSQL TCP 5432, and mock TCP 8081.

This does **not** prove exclusive mock ingress: the pre-existing shared-sandbox `allow-same-namespace` policy selects all pods and permits all same-namespace pod ingress. Kubernetes NetworkPolicy allowances are additive. The baseline policy was not mutated, and no unauthorized namespace was created.

Exact scans found zero credential or HITL-plaintext matches across 18,602 current/previous log bytes and a 242,443-byte PostgreSQL data dump (`sha256:c74f8de4d686575e0125046be7b3a30267a6b04dc8274ab63295c323e71ffe33`).

Docker Scout v1.24.0 scanned the exact local v7 OCI index on 2026-08-10. It reported five unfixed findings and zero fixable findings:

- CRITICAL: `CVE-2026-12087`, `CVE-2026-13221` in Debian 13 `perl-base`.
- HIGH: `CVE-2026-48959`, `CVE-2026-48962` in Debian 13 `perl-base`.
- HIGH: `CVE-2026-66032` in `libssh2`, introduced with canary-only `curl`.

Four findings inherit from the pinned Debian 13 base. None were suppressed. This unpublished disposable canary was not rebuilt after acceptance; base-image and canary-container supply-chain hardening remains a separate follow-up.

## Cleanup proof

The original `ic007f_` store had exactly ten tables at cleanup and the earlier
`ic007q_` quota store had already been reduced to zero. The rotation rerun had
three Success rows, zero pending mailbox rows, and exactly ten `ic007r_`
tables. Apps were drained first; the two exact ten-table sets were dropped by
name and each attributable prefix then counted zero.

Only explicitly named objects under
`ironcrew.io/test-run=ic007-final-20260810` and
`ironcrew.io/test-run=ic007-rotation-rerun-20260810` were removed. Both
selectors now return zero objects. The remaining namespace has no workloads,
only the pre-existing `modelmesh-serving` Service, the same ten baseline
NetworkPolicies, seven ConfigMaps, and six Secrets recorded before testing.
All four ResourceQuota objects report zero usage.

Locally, 17 attributable Docker tags and ten test image objects, three exact
staging directories, 14 temporary Python harnesses, and seven attributable
bytecode files were removed. `postgres:15` was retained as the current
minimum-major test image. No Docker prune was used. Registry login used a
private temporary config, logout completed, no such temp directory remains,
and the accidental workspace registry-auth filename is absent. The active
OpenShift OAuth session was intentionally retained.

## Evidence and retention boundary

The exact artifact was retained through local and platform verification. After requested cleanup, no 20+ MiB binary/OCI copy is retained as a tracked receipt. The digests prove observed identity only; the dirty-worktree build manifest is not claimed to be bit-reproducible or downloadable.

Machine-readable evidence:

- `ic007-openshift-v7.json`
- `ic007-openshift-v7-effective-config.json`
- `ic007-openshift-v7-rotation.json`
- `ic007-v7-assembly.Dockerfile`
- `ic007-v7-build-attestation.json`
- `ic007-v7-build-manifest.json`
- `ic007-v7-assembly-context.json`

Rejected precursor probes caused by harness answer literals, SQL formatting, EndpointSlice/journal synchronization, and an admission-bursting route sampler are not counted as acceptance. Their attributable runs reached terminal state with no lingering mailbox rows; the final evidence above came from corrected bounded probes.
