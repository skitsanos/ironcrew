---
name: rust-release-workflow
description: Prepare and cut an IronCrew release through develop and main, including PR review, complete validation, versioning, tagging, and workflow monitoring. Use only with explicit release intent.
---

# IronCrew release workflow

Use explicit user approval before merges, remote PR changes, commits, pushes,
version changes, tags, releases, images, or crates.io publication. Never
force-push, rewrite published history, or delete a remote tag.

## Branch model

- `develop` is the integration branch.
- `main` is the release branch.
- The flow is `develop` to a reviewed `main` PR, then an annotated tag on the
  verified `main` commit. Tags use the stable `vX.Y.Z` form. A versioned
  `repository_dispatch` selects the release workflow from default-branch
  `main`; the workflow separately checks direct annotation-to-commit type,
  manifest version, and reachability from `main`.

Default-branch dispatch selection and the checked-in guard are defense in depth,
not the complete platform boundary. Require reviewed default-branch controls
and a protected `release` environment before treating publication as
platform-enforced.

The current repository has no ruleset, environment, or `main` protection, and
its only direct collaborator is the owner. Do not dispatch either workflow
until an independent environment reviewer exists, self-approval and bypass are
disabled, Docker credentials have moved from repository secrets into the
`release` environment, protected `v*` tag rules cover update/deletion and
creation
without administrator bypass, immutable releases are enabled, and the
non-secret controls have been revalidated. Repository dispatch requires
Contents write, so use a dedicated constrained release authority or lower-
authority request channel backed by trusted platform actor/event policy; sender
equality is not an operator allowlist.

Default-branch dispatch does not stop an off-main commit from introducing a
different tag-push workflow. Require GitHub workflow-execution protections or
an equivalent platform boundary for untrusted actors/events. Coordinate that
policy with CI because `ci.yml` still intentionally uses push events on `main`
and `develop`.

## Prepare

1. Require a clean worktree. Record branch, manifest/lock versions, latest tag,
   and divergence from `origin/develop` and `origin/main`.
2. List open pull requests targeting `develop`; summarize mergeability, review,
   and CI. Ask before merging, closing, or otherwise mutating them.
3. Review dependency drift and `cargo audit --deny warnings`. Ask before incompatible upgrades
   or audit-policy changes.
4. Run the complete `/check` gate, including required PostgreSQL and replica
   evidence. Do not proceed on a skipped required test. Treat unavailable
   platform runners as CI evidence, not as locally executed checks.
5. Read commits and resolved `IC-NNN` records since the latest tag. Propose a
   SemVer bump and release notes from actual behavior, then wait for approval.

## Release

After approval:

1. Update `Cargo.toml`, refresh the root package version in `Cargo.lock`, and
   read both back.
2. Rerun the complete gate. Stage only explicit release files; commit and push
   `develop` only when authorized.
3. Open or update the `develop` to `main` release PR. Wait for green CI and
   approval; merge only with user authorization.
4. Fast-forward local `main`, create an annotated `vX.Y.Z` on the verified
   release commit, prove it is reachable from `main`, and prove its manifest
   version matches.
5. Show the evidence and ask before pushing the tag. Pushing the tag does not
   publish a release. Once the remote controls above exist, separately ask to
   exercise the protected environment without publication. Replace the tag in
   this exact request with the pushed stable tag:

   ```bash
   gh api --method POST repos/skitsanos/ironcrew/dispatches --input - <<'JSON'
   {"event_type":"ironcrew_release_v1","client_payload":{"tag":"vX.Y.Z","mode":"validate"}}
   JSON
   ```

   Invoke this only through the constrained release authority described above.
   Confirm independent approval and a successful read-only run with no OIDC,
   release, Docker-secret, or registry operation. Capture a separately designed
   denied adversarial canary; `repository_dispatch` always selects the default-
   branch workflow and cannot be pointed at an off-main ref.
6. Only after those canaries and separate explicit publication approval, use:

   ```bash
   gh api --method POST repos/skitsanos/ironcrew/dispatches --input - <<'JSON'
   {"event_type":"ironcrew_release_v1","client_payload":{"tag":"vX.Y.Z","mode":"publish"}}
   JSON
   ```

   A release created with the workflow's `GITHUB_TOKEN` does not cascade into
   another release-event workflow; image promotion remains separate.
   Monitor `release.yml`. Its read-only image job must build the signed multi-
   platform OCI archive and strict receipt from the exact tag commit. The final
   protected job must verify those immutable artifacts, sign them, and create
   the release once without updating existing release assets.
7. Image promotion through `.github/workflows/docker-publish.yml` requires
   separate authorization. After the protected environment exists, its non-
   publishing path can be checked with:

   ```bash
   gh api --method POST repos/skitsanos/ironcrew/dispatches --input - <<'JSON'
   {"event_type":"ironcrew_docker_publish_v1","client_payload":{"tag":"vX.Y.Z","mode":"validate"}}
   JSON
   ```

   Before actual promotion, require the exact Docker Hub stable-semver
   immutability rule and IC-015's recorded non-production replay/conflict/
   concurrent-`latest` acceptance. While IC-015 remains in progress, keep
   Docker publication deferred. Once it is resolved and the user separately
   authorizes production promotion, use:

   ```bash
   gh api --method POST repos/skitsanos/ironcrew/dispatches --input - <<'JSON'
   {"event_type":"ironcrew_docker_publish_v1","client_payload":{"tag":"vX.Y.Z","mode":"publish"}}
   JSON
   ```

   It verifies and promotes tag-owned assets instead of rebuilding source.
   Monitor the version digest and final `latest` digest against GitHub's current
   stable release. Publish crates only with separate authorization too.
   Do not backfill a legacy release that lacks the signed OCI archive and
   receipt; promotion begins with the first release produced by the trusted
   default-branch workflow.

If an unpushed local step is wrong, make a corrective edit or ask for direction.
After a push, prefer a forward fix. Do not use destructive reset or broad
implicit staging as release recovery.
