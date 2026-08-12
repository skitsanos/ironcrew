---
name: release-ironcrew
description: Prepare, verify, version, and publish an IronCrew release through develop and main with explicit approval gates. Use when the user asks to release, cut a release, ship a version, bump the product version for release, or integrate dependency work into a release. Do not use for routine dependency updates, PR review alone, or deployment without release intent.
---

# Release IronCrew

Use explicit user approval before merges, remote PR changes, commits, pushes,
version changes, tags, releases, images, or crates.io publication. Never
force-push, rewrite a published commit, or delete a remote tag.

## Establish release state

1. Require a clean worktree. Record the current branch, `Cargo.toml` and
   `Cargo.lock` versions, latest `v*` tag, and divergence from remote
   `develop` and `main`.
2. Confirm the repository still integrates on `develop` and releases from
   `main`. A release tag must be an annotated stable `vX.Y.Z` tag whose commit
   is reachable from `main`.
3. Stop if unrelated changes would enter the release or version/tag history is
   inconsistent. Do not repair history destructively.
4. List open pull requests targeting `develop` and summarize mergeability,
   review state, and CI. Ask before changing remote state.

Release and Docker publication are admitted through versioned
`repository_dispatch` events so GitHub selects the workflow from the default
branch rather than from a tag commit or caller-selected workflow ref. That
selection is still only repository-level defense in depth. Require reviewed
default-branch controls and a protected `release` environment before treating
publication as platform-enforced.

The current repository has no ruleset, environment, or `main` protection, and
its only direct collaborator is the owner. Do not dispatch either workflow
until an independent environment reviewer exists, self-approval and bypass are
disabled, Docker credentials have moved from repository secrets into the
`release` environment, protected `v*` tag rules prevent update/deletion and
restrict creation without administrator bypass, immutable releases are enabled,
and the documented non-secret controls have been revalidated. Because creating
a repository dispatch requires Contents write, use only a dedicated constrained
release authority or a lower-authority request channel backed by a trusted
controller and platform actor/event policy; do not call sender/actor equality an
operator allowlist. `validate` mode is non-publishing, but it still must wait for
those controls because its purpose is to prove their enforcement.

Default-branch dispatch does not prevent a tag-capable actor from adding a
different tag-push workflow to an off-main commit. Require GitHub workflow-
execution protections, or an equivalent platform boundary, that denies such
untrusted actors/events. Coordinate this with CI: `.github/workflows/ci.yml`
still intentionally uses `push` for `main` and `develop`, so do not blanket-
disable push events without an agreed CI-trigger replacement.

## Verify and propose

1. Review dependency drift and `cargo audit --deny warnings`; ask before incompatible major
   upgrades or audit-policy changes.
2. Use `$check-ironcrew` to run the complete locally reproducible repository,
   Rust, Lua, evaluation, PostgreSQL, replica, and release-build gates. Required
   live tests may not be skipped. Platform-only jobs remain a later CI gate.
3. Read commits and resolved `IC-NNN` entries since the latest tag. Propose a
   Semantic Versioning bump from behavior, not commit-message prefixes alone.
4. Draft release notes grouped by area and referencing relevant issue IDs.
   Wait for the user's version approval.

## Cut the release

After approval:

1. On `develop`, update the package version in `Cargo.toml`, refresh the exact
   root package entry in `Cargo.lock`, and read both back.
2. Rerun the complete gate. Stage only explicit release paths and commit/push
   only when authorized.
3. Open or update the `develop` to `main` release PR. Wait for green CI and the
   required review, then merge only with user authorization.
4. Fast-forward local `main` from `origin/main`. Create an annotated `vX.Y.Z`
   tag only on the verified release commit.
5. Prove the tag is reachable from `main` and its `Cargo.toml` contains the same
   version. Show the evidence and ask before pushing the tag.
6. Push the tag only with explicit approval. Pushing it does not publish a
   release. After the remote controls above exist, first ask for authorization
   to exercise the protected environment without publication. Replace the tag
   in this exact request with the pushed stable tag:

   ```bash
   gh api --method POST repos/skitsanos/ironcrew/dispatches --input - <<'JSON'
   {"event_type":"ironcrew_release_v1","client_payload":{"tag":"vX.Y.Z","mode":"validate"}}
   JSON
   ```

   The payload must contain exactly `event_type` plus `client_payload`, and the
   client payload must contain exactly `tag` and `mode`. Invoke it only with the
   constrained release authority described above. Confirm that the
   protected `release` environment required independent approval and that the
   run completed without contents-write, OIDC signing, a GitHub release, or
   registry access. Capture a separately designed denied adversarial canary;
   `repository_dispatch` itself always selects the default-branch workflow and
   cannot be pointed at an off-main ref.
7. Only after those canaries and a separate explicit publication approval,
   dispatch the exact release request:

   ```bash
   gh api --method POST repos/skitsanos/ironcrew/dispatches --input - <<'JSON'
   {"event_type":"ironcrew_release_v1","client_payload":{"tag":"vX.Y.Z","mode":"publish"}}
   JSON
   ```

   A release created with the workflow's `GITHUB_TOKEN` does not cascade into
   another release-event workflow; Docker promotion remains the separately
   authorized dispatch below.
   Monitor `.github/workflows/release.yml`. It must create the release once and
   publish the signed OCI archive plus strict image receipt; it must not update
   an existing release or replace its assets.
8. Docker publication through `.github/workflows/docker-publish.yml` is a
   separate authorization. It verifies and promotes the tag-owned signed
   archive rather than rebuilding source. Before any Docker
   publication, require the exact Docker Hub stable-semver immutability rule and
   IC-015's recorded non-production replay/conflict/concurrent-`latest`
   acceptance. After the protected environment exists, its non-publishing
   Docker path can be checked with a separate authorization and this exact
   request; it must not read Docker credentials or touch the registry:

   ```bash
   gh api --method POST repos/skitsanos/ironcrew/dispatches --input - <<'JSON'
   {"event_type":"ironcrew_docker_publish_v1","client_payload":{"tag":"vX.Y.Z","mode":"validate"}}
   JSON
   ```

   While IC-015 remains in progress, keep actual Docker publication deferred.
   Once IC-015 is resolved and the user separately authorizes production
   promotion, use this exact request:

   ```bash
   gh api --method POST repos/skitsanos/ironcrew/dispatches --input - <<'JSON'
   {"event_type":"ironcrew_docker_publish_v1","client_payload":{"tag":"vX.Y.Z","mode":"publish"}}
   JSON
   ```

   Monitor the version digest and final `latest` digest against GitHub's current
   stable release. Publish a crate only with separate authorization too.
   Do not use this path to backfill a legacy release without the signed OCI
   archive and receipt; promotion begins with the first release produced by the
   trusted default-branch release workflow.

If an unpushed local release step is wrong, make a corrective edit or ask for
direction. Once anything is pushed, use a forward fix unless the user explicitly
authorizes another safe recovery.
