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
  verified `main` commit. Tags use the stable `vX.Y.Z` form. The tag-triggered
  workflow checks annotation type, manifest version, and reachability from
  `main`.

The checked-in guard is defense in depth because GitHub evaluates push
workflows from the pushed ref. Require protected `v*` tag rules and a protected
release environment before treating publication as platform-enforced.

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
5. Show the evidence and ask before pushing the tag. Monitor release CI. A
   release created with `GITHUB_TOKEN` does not cascade into a release-event
   workflow, so image publication requires a separate, explicitly authorized
   manual dispatch of `docker-publish.yml` from `main` with the exact tag.
   IC-015 remains open because a repeated dispatch makes an unconditional
   version-tag push without a repository-side existing-digest guard, and the
   `latest` update has a time-of-check/time-of-use race. While IC-015 is open,
   stop before that dispatch and keep Docker image publication deferred; do not
   describe its aliases as immutable or rollback-proof. Publish crates only
   with separate authorization too.

If an unpushed local step is wrong, make a corrective edit or ask for direction.
After a push, prefer a forward fix. Do not use destructive reset or broad
implicit staging as release recovery.
