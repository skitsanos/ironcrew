#!/usr/bin/env bash
set -euo pipefail

tag_name="${1:-${GITHUB_REF_NAME:-}}"
main_ref="${2:-refs/remotes/origin/main}"

if [[ -z "$tag_name" ]]; then
  echo "release source verification: tag name is required" >&2
  exit 1
fi

if [[ ! "$tag_name" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
  echo "release source verification: tag $tag_name must match annotated stable tag form vX.Y.Z" >&2
  exit 1
fi

tag_ref="refs/tags/$tag_name"
tag_type=$(git cat-file -t "$tag_ref" 2>/dev/null || true)
if [[ "$tag_type" != "tag" ]]; then
  echo "release source verification: tag $tag_name must be an annotated tag" >&2
  exit 1
fi

if [[ ! -f Cargo.toml ]]; then
  echo "release source verification: Cargo.toml is missing" >&2
  exit 1
fi

tag_version="${tag_name#v}"
crate_version=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
if [[ -z "$crate_version" || "$tag_version" != "$crate_version" ]]; then
  echo "release source verification: tag $tag_name does not match Cargo.toml version $crate_version" >&2
  exit 1
fi

tag_commit=$(git rev-parse "$tag_ref^{commit}")
if [[ -z "$tag_commit" ]]; then
  echo "release source verification: cannot resolve tag $tag_name" >&2
  exit 1
fi

if ! git rev-parse --verify --quiet "$main_ref^{commit}" >/dev/null; then
  echo "release source verification: cannot resolve release branch ref $main_ref" >&2
  exit 1
fi

if ! git merge-base --is-ancestor "$tag_commit" "$main_ref"; then
  echo "release source verification: tag $tag_name points to $tag_commit, which is not reachable from $main_ref" >&2
  exit 1
fi

echo "Release source verified: $tag_name matches $crate_version and is reachable from $main_ref."
