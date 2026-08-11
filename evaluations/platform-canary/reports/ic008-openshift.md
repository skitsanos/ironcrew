# IC-008 OpenShift conversation canary receipt

Recorded 2026-08-11 against the temporary `gedankrayze-dev` namespace and
selector `ironcrew.io/test-run=ic008-conversation-20260811`.

## Outcome

The exact unpublished IC-008 artifact passed the applicable real OpenShift
multi-replica conversation matrix through affinity-free Routes. The accepted
sequence proved cold rehydration, same-key replay without another mock effect,
durable history through either process, between-turn owner removal, replacement
process rehydration, prompt same-process and peer-process active-delete fences,
delete/recreate incarnation fencing, and final durable-ledger invariants.

This is OpenShift evidence only. Railway was not repeated. The canary does not
claim in-flight Lua/provider/tool takeover, shared-store conversation SSE,
general exactly-once external effects, a released artifact, bit-reproducibility,
or exclusive namespace network isolation.

Cleanup is complete. The exact database prefix, labeled platform objects,
temporary artifact copies, attributable Docker objects, and Python caches are
gone; the namespace baseline and zero quota usage were restored.

## Exact artifact and configuration

- Base commit: `c4799a3c3b8a2441243ad512436d1cb649275cf4`
- Dirty build-input manifest: `sha256:fdf9b1813eaf914f03089931c1016134983c9c1c7ce622e85bbd32ae6eb2414e` (373 inputs)
- Linux/amd64 binary: `sha256:1cb77b4f712381a8aa2226fd1963f576bccb54d350329cf88e420806d4e0c4f3`
- Flow tree: `sha256:4f0f32aba78d2b571d4052a962ff5798c10a6f65f389a211946381c0958aa7de`
- Effective config: `sha256:5d7411e95639c5cbcf9cc6cfe5005895f8ebd19fd794415619d5f2d54e295d12` (113 fields)
- Empty HITL keyring: `sha256:9bc1cc42c4690cde8d2f666c9e572f81cd20c2341ff20cc1bac4d0744601e155`
- Deployment revision: `git:c4799a3c3b8a2441243ad512436d1cb649275cf4+manifest:fdf9b1813eaf914f03089931c1016134983c9c1c7ce622e85bbd32ae6eb2414e`
- OCI manifest: `sha256:ad413aff04e3eae80c5c3b82e3b03e9387c5a15809e24a94992804f14e4ac29a`
- OCI config: `sha256:5f227b981026bdee84fbec487f6f8f3aad5b311f3f7e7d0e56c532665cd2a0e2`
- Image size: 54,044,299 bytes
- OpenShift 4.21.21 / Kubernetes 1.34.8
- Mock Pod `1c862b6f-1a61-4cc4-8cf1-802479495e7b`: spec and runtime image `sha256:ad413aff04e3eae80c5c3b82e3b03e9387c5a15809e24a94992804f14e4ac29a`, restart count zero
- PostgreSQL Pod `c076f572-dc4a-4227-8b4d-d6b3b2777e45`: spec and runtime image `quay.io/sclorg/postgresql-15-c9s@sha256:68bdb875e869aabc8e8cd1a62bf9a7ff0e46142342ce81c5a80156f3fead7263`, resolved Linux/amd64 manifest `sha256:d675aee51ccd1a4b7f557848336cf5b7e624d040c934fcb8e68234114729f091`, server 15.12, restart count zero
- Conversation bootstrap definition: agent `coordinator`, `max_history` 20

The build attestation was generated again after the build and remained
byte-identical, proving that the retained source-input manifest did not change
during assembly. The nine-file assembly inventory binds the binary, flow,
Dockerfile, attestation, and all five verification/mock helpers.

## Process inventory and routing

| Role | Pod UID / instance ID | Process start ID | Direct Route | Result |
| --- | --- | --- | --- | --- |
| Initial A | `a49fb7cf-ec09-4555-8776-6bdd70284727` | `e1119896-86a3-4c5e-b59d-d7ed3e7c65a9` | `ic008-conv-20260811-a` | Attested, then force-deleted between committed turns |
| B | `ff0a6ebc-066a-4293-9f1e-0295a0ca5ae7` | `339c03dd-597c-4df7-b99c-023af8c86f51` | `ic008-conv-20260811-b` | Attested, retained through the primary matrix, then force-deleted after its supplemental committed owner turn |
| Replacement C | `91bc1795-4b26-4d45-8d32-8dc0daaadf4e` | `1dd972c2-73f3-4eeb-b963-7b49a639fc70` | `ic008-conv-20260811-a` | Distinct replacement, independently attested |

Every process had restart count zero and independently matched the PID 1
executable, binary, flow, 113-field config, empty keyring, build manifest, and
five helper hashes. Its authenticated capability receiver, body instance ID,
and process-start ID all matched the exact Pod UID. API-token and database-URL
presence were checked without retaining either value; HITL keys and an OpenAI
key were absent.

The initial shared Route returned 64/64 valid capability responses with an
exact 32/32 A/B split. The replacement Route returned 32/32 valid responses
with an exact 16/16 B/C split. During the owner-absent barrier, 16/16 requests
reached B only. Services used `sessionAffinity: None`; Routes used round-robin
balancing with cookies disabled.

## Conversation matrix

Conversation `ic008-ocp-20260811` began with incarnation
`19d8b40e-0480-41d3-be54-fadc2fba6c7b`, source fingerprint
`sha256:f88689b58f330ecfe624c8e69c430df860c41aa4e65c8b94153d30042122b207`,
and definition fingerprint
`sha256:c4311818560bf5b3882654e7da62ce142dc1de63e2b733181a934ffd712c5920`.

| Case | Evidence | Result |
| --- | --- | --- |
| Start through both processes | Direct A and B returned byte-equivalent revision-1 identity with zero provider calls. | Pass |
| Key requirement | A message without `Idempotency-Key` returned 400 with zero provider calls. | Pass |
| First turn and replay | B executed K1; A and both shared-Route receivers replayed the exact response with `Idempotency-Replayed: true`. Mock counts stayed 2 chat / 1 effect / 1 final / 1 tool-call. | Pass |
| Durable history and SSE boundary | A and B returned byte-equivalent history. Both receivers returned the specified 409 explaining that shared-store SSE replay is unavailable and durable history is the recovery surface. | Pass / intentional SSE boundary |
| Between-turn owner removal | OpenShift removed A and EndpointSlice withdrew it. B alone remained ready and committed K2 at revision 3; counts became 4 / 2 / 2 / 2. | Pass |
| Replacement process | C had a distinct Pod UID and process-start ID, rehydrated revision-3 history without an effect, and replayed K2 with B through the Route. | Pass |
| Concurrent K3 | While B was blocked after the first provider request, C's same-key duplicate returned 409. DELETE through B returned 409 in 378 ms; DELETE through C returned 409 in 412 ms. After one release, B committed revision 4 and C replayed it exactly. Counts became 6 / 3 / 3 / 3. | Pass |
| Delete/recreate fence | C deleted the conversation. B recreated it as incarnation `020fa409-2e48-4682-a773-539907dc881e` with stable source/definition identity. Reusing old K1 returned 409 without an effect; B performed the final delete. | Pass |

All protected responses asserted the expected receiver, JSON content type, and
`Cache-Control: no-store`. A 17-record bounded sanitized receipt separately
retains an exact 4/4 B/C Route split, sanitized-equivalent B/C start,
sanitized-equivalent first/replayed message and history with equal response
sizes, the 409 SSE boundary on both receivers, and delete through C. The
sanitizer deliberately drops response content that is unsafe to retain, so
this supplemental receipt does not claim raw-body byte equality. That probe advanced counts from 6 / 3 / 3 / 3 to
8 / 4 / 4 / 4 and was deleted terminally.

The final PostgreSQL state contained zero conversations and six completed 200
`conversation.message` ledger rows, zero active rows, six distinct
64-character key hashes, and 36-character attempt IDs. The retained row
inventory binds each resource, base revision, response size, and owner process.
The ten expected tables, eighteen indexes, and two accounting functions were
inventoried before cleanup.

## Owner-loss boundary

An attempted in-container `SIGKILL` of namespace PID 1 was a platform-injection
no-op: namespace-init protection left the exact process unchanged. It is not
counted. The accepted proof used exact OpenShift Pod force deletion after K1
had committed and before K2 began. This validates recovery between committed
turns; it deliberately does not claim takeover of an in-flight VM or external
effect.

The primary sequence's K2 executor B already had the conversation cached, so a
second scoped proof closed the cold-rehydration boundary. B alone started
`ic008-cold-recovery-20260811` and committed revision 2. C had zero active
conversation handles and had made no operation for that ID. B's exact Pod UID
was then force-deleted and withdrawn from the app EndpointSlice. Without a
preceding `/start`, history read, or message for that ID, C accepted a new keyed
turn directly from PostgreSQL, retained the same incarnation/source/definition,
and committed revision 3. The mock delta for that cold turn was exactly
2 / 1 / 1 / 1; C then returned revision-3 history and deleted the conversation.
The five sanitized operation records are retained separately.

## Security, resources, and known limitations

The app processes ran under `restricted-v2` as UID `1004800000`, RuntimeDefault
seccomp, read-only root filesystems, no privilege escalation, all capabilities
dropped, and no service-account token. Root writes failed and the bounded
`/tmp` probe succeeded. Last-observed B RSS before deletion was 18,432 KiB;
final C RSS was 18,304 KiB, with thirteen file descriptors each. Peak canary requests were 400m CPU / 512 MiB memory;
limits were 4 CPU / 2 GiB memory; no PVC was created.

Exact scans found zero matches for the three live secret values across 20,182
pre-supplemental B/C/mock/PostgreSQL log bytes, 11,329 post-supplemental
surviving C/mock/PostgreSQL log bytes, a 38,494-byte final database dump,
92,326 bytes of non-Secret resource JSON, 8,968 bytes of image
configuration/history, 26,127,702 assembly bytes, and the final retained
platform-file set. Those scoped logs had zero classified application errors
or warnings. Initial A and B were deleted before their final log tails could be
rescanned; no secret-clean claim is made for those unavailable tail bytes.

Docker Scout 1.24.0 found four unfixed Debian-base findings in `perl 5.40.1-6`:
CRITICAL `CVE-2026-12087` and `CVE-2026-13221`, plus HIGH
`CVE-2026-48959` and `CVE-2026-48962`. The fixed-only count was zero. These
findings are retained rather than suppressed; the disposable canary was not
rebuilt after acceptance.

The two canary NetworkPolicies express the intended app-to-PostgreSQL/mock
paths, but the namespace's pre-existing additive `allow-same-namespace` policy
also permits same-namespace traffic. Exclusive ingress isolation is therefore
not claimed.

The mock control endpoints are intentionally unauthenticated but had no public
Route. Every counted phase observed zero non-test Pods and zero Deployments,
StatefulSets, DaemonSets, Jobs, or CronJobs; only the exact labeled app, mock,
and PostgreSQL workloads existed. Mock deltas are therefore attributed to that
bounded workload inventory, not to an exclusive-NetworkPolicy claim.

## Excluded fixture attempts

- The initial SCL PostgreSQL mount exposed only `/var/lib/pgsql/data`; the image
  also needed writable `/var/lib/pgsql`. It failed before functional traffic.
- The first parallel A/B startup exhausted probes while PostgreSQL was
  unavailable. Both pods were recreated serially before functional traffic.
- The namespace-PID-1 signal attempt was the no-op described above. The accepted
  owner-loss proof is the exact Pod deletion, not that signal attempt.

## Cleanup and retained evidence

Cleanup completed at `2026-08-11T08:04:28Z`. After the last app Pod was
deleted, four idle connections from deleted app IPs remained beyond the bounded
drain wait; only those dedicated canary-role sessions were terminated, and the
remaining count was zero. The ten exact `ic008c_20260811_` tables and two exact
functions were then dropped by name without `CASCADE`; both prefix counts were
zero before PostgreSQL was deleted.

Only explicitly named canary Pods, Services, Routes, NetworkPolicies,
ConfigMap, Secret, and ImageStream were deleted. The selector now returns zero
objects. The remaining namespace exactly matches the canonical preflight
inventory (`sha256:ce9697dfb8eb519641338240dcbb0ab328952ebc8b07c9500a511101d774d4dd`):
zero Pods/workload controllers/Routes/PVCs/ImageStreams; Service
`modelmesh-serving`; the same seven ConfigMaps, six Secrets, ten
NetworkPolicies, and five ServiceAccounts; and zero usage in each declared
ResourceQuota dimension. The namespace, project, and authenticated OpenShift
session were preserved.

Locally, the exact staging directory, attributable Python cache and bytecode,
three IC-008 image references, two pinned SCL pull objects, and all attributable
containers/images are gone. `postgres:15` remains at
`sha256:6eb0add3b77c081df18aa518ce43df58fdcc40f2e6d868a6fd08038dc7acd425`.
No Docker prune or broad platform deletion was used. The ImageStream was
removed; no claim is made about the registry's asynchronous blob garbage
collection.

Machine-readable evidence:

- `ic008-openshift.json`
- `ic008-openshift-effective-config.json`
- `ic008-openshift-build-attestation.json`
- `ic008-openshift-assembly-context.json`
- `ic008-openshift-functional.json`
- `ic008-openshift-cold-recovery.json`
- `../ic008-openshift-template.yaml`

The three local HTTP/receipt helpers are also content-addressed in the machine
receipt. The orchestration wrapper was executed inline and its exact wrapper
bytes were not retained; that is an explicit ad-hoc controller boundary. Both
sanitized receipts round-trip exactly through the retained sanitizer.

The OCI artifact is unpublished and disposable. Its retained manifests prove
the tested identity but do not promise a downloadable or bit-reproducible build.
