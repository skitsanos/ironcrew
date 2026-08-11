# IC-007 Railway v6 cancelled preflight

Status: cancelled before functional testing. V6 remains negative operator/preflight evidence only; the corrected final artifact is named v7.

The first Railway upload used the intended v6 staging directory after a local fingerprint probe had created one Python bytecode cache that was not present in the already-built authoritative OpenShift image. Railway therefore built digest `sha256:31adc25d2925a0f43012527e5934efbb24ac9958d0422b7e86e3905d385b8e2d` from an 11-file context. No shared OCI identity or runtime parity is claimed. The extra 3,572-byte pycache was positively attributed, removed, and excluded from the retained canonical 10-file assembly-context inventory.

Two Railway instances reached startup and created the fresh v6 schema, but no functional request was sent. A concurrent source review found that the new transaction timeout-configuration failure would not enter the retry classifier, so the exact deployment was cancelled through the GraphQL `deploymentCancel(id)` mutation and both instances became `REMOVED`.

The attempted CLI `railway down` cancellation instead removed the previous healthy v5 deployment while leaving the pending v6 deployment active. Because this is an isolated sandbox, the application service is intentionally left quiescent until v7; no v5 restoration was attempted. This CLI behavior is retained as an operator note and does not broaden product scope.

The empty v6 prefix currently contains only initialized schema objects: ten tables, eighteen indexes, and two functions, with zero run, idempotency, mailbox, event, or event-state rows. It remains isolated alongside the v5 negative prefix and will be dropped explicitly during final cleanup.
