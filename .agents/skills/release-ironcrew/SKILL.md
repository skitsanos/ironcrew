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

The checked-in ancestry guard is defense in depth, not an adversarial trust
boundary: GitHub evaluates a tag-push workflow from the tagged commit. Require
protected `v*` tag rules and a protected release environment before treating
publication as platform-enforced.

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
6. Push the tag and monitor `.github/workflows/release.yml`. A release created
   with `GITHUB_TOKEN` does not trigger a second release-event workflow. The
   release workflow must create the release once and publish the signed OCI
   archive plus strict image receipt; it must not update an existing release or
   replace its assets. Image publication is a separate, explicitly authorized
   manual dispatch of `.github/workflows/docker-publish.yml` from `main` with
   the exact tag and latest-reconciliation boolean. It verifies and promotes
   the tag-owned signed archive rather than rebuilding source. Before dispatch,
   require the exact Docker Hub stable-semver immutability rule and IC-015's
   recorded non-production replay/conflict/concurrent-`latest` acceptance. While
   IC-015 remains in progress, stop before dispatch and keep Docker publication
   deferred. After authorization, monitor the version digest and final `latest`
   digest against GitHub's current stable release. Publish a crate only with
   separate authorization too.
   Do not use this path to backfill a legacy release without the signed OCI
   archive and receipt; promotion begins with the first release produced by the
   new tag workflow.

If an unpushed local release step is wrong, make a corrective edit or ask for
direction. Once anything is pushed, use a forward fix unless the user explicitly
authorizes another safe recovery.
