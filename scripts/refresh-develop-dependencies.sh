#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

require_command() {
  local command_name=$1
  local install_hint=$2

  if ! command -v "$command_name" >/dev/null 2>&1; then
    printf 'develop-refresh: required command not found: %s\n' "$command_name" >&2
    printf 'develop-refresh: %s\n' "$install_hint" >&2
    exit 1
  fi
}

run() {
  printf 'develop-refresh: running %s\n' "$1"
  shift
  "$@"
}

latest_cargo_crate_version() {
  local crate_name=$1
  cargo search "$crate_name" --limit 1 \
    | awk -F'"' -v prefix="$crate_name = " '$0 ~ "^" prefix { print $2; exit }'
}

ensure_latest_cargo_tool() {
  local crate_name=$1
  local binary_name=$2
  local latest_version
  local installed_version=""

  latest_version=$(latest_cargo_crate_version "$crate_name")
  if [[ -z "$latest_version" ]]; then
    printf 'develop-refresh: could not resolve latest %s version\n' "$crate_name" >&2
    exit 1
  fi
  installed_version=$(
    cargo install --list \
      | awk -v prefix="$crate_name v" \
          'index($0, prefix) == 1 { value = $0; sub(":$", "", value); sub(prefix, "", value); print value; exit }'
  )
  if [[ "$installed_version" != "$latest_version" ]]; then
    run "$crate_name $latest_version install" \
      cargo install "$crate_name" --version "$latest_version" --locked --force
  fi
  require_command "$binary_name" "Install $crate_name before refreshing IronCrew dependencies."
  printf 'develop-refresh: %s %s\n' "$crate_name" "$latest_version"
}

require_command git "Install Git before refreshing IronCrew dependencies."
require_command python3 "Install Python 3 before refreshing IronCrew dependencies."
require_command curl "Install curl before refreshing IronCrew dependencies."
require_command bun "Install Bun before refreshing IronCrew dependencies."
require_command cargo "Install Rust through rustup before refreshing IronCrew dependencies."
require_command rustup "Install rustup before refreshing the Rust stable toolchain."
require_command actionlint "Install actionlint before refreshing workflow tooling."

run "latest stable Bun" bun upgrade --stable
printf 'develop-refresh: Bun %s (%s)\n' "$(bun --version)" "$(bun --revision)"

run "latest stable Rust" rustup update stable --no-self-update
pinned_rust=$(awk -F'"' '/^channel = / { print $2; exit }' rust-toolchain.toml)
stable_rust=$(rustup run stable rustc --version | awk '{ print $2 }')
if [[ "$pinned_rust" != "$stable_rust" ]]; then
  printf '%s\n' \
    "develop-refresh: repository Rust $pinned_rust is behind stable $stable_rust." \
    'develop-refresh: update the Rust manifest, toolchain, Docker, workflow, policy, and docs pins together.' >&2
  exit 1
fi
printf 'develop-refresh: Rust stable matches repository pin %s\n' "$pinned_rust"

ensure_latest_cargo_tool "cargo-audit" "cargo-audit"
ensure_latest_cargo_tool "cargo-edit" "cargo-upgrade"
ensure_latest_cargo_tool "cargo-outdated" "cargo-outdated"

latest_cargo_audit=$(cargo-audit --version | awk 'NR == 1 { print $2 }')
if ! grep -Fq "cargo-audit --version $latest_cargo_audit --locked" .github/workflows/ci.yml; then
  printf '%s\n' \
    "develop-refresh: CI does not install latest cargo-audit $latest_cargo_audit." \
    'develop-refresh: update the workflow and repository policy together.' >&2
  exit 1
fi

latest_actionlint=$(
  curl -fsSL https://api.github.com/repos/rhysd/actionlint/releases/latest \
    | python3 -c 'import json, sys; print(json.load(sys.stdin)["tag_name"].removeprefix("v"))'
)
installed_actionlint=$(actionlint -version | awk 'NR == 1 { print $1 }')
if [[ "$installed_actionlint" != "$latest_actionlint" ]]; then
  if command -v brew >/dev/null 2>&1 && brew list --versions actionlint >/dev/null 2>&1; then
    run "actionlint $latest_actionlint upgrade" brew upgrade actionlint
    installed_actionlint=$(actionlint -version | awk 'NR == 1 { print $1 }')
  fi
fi
if [[ "$installed_actionlint" != "$latest_actionlint" ]]; then
  printf '%s\n' \
    "develop-refresh: actionlint $installed_actionlint is behind $latest_actionlint." \
    'develop-refresh: upgrade actionlint with its package manager and retry.' >&2
  exit 1
fi
if ! grep -Fq "actionlint/cmd/actionlint@v$latest_actionlint" .github/workflows/ci.yml; then
  printf '%s\n' \
    "develop-refresh: CI does not install latest actionlint v$latest_actionlint." \
    'develop-refresh: update the workflow and repository policy together.' >&2
  exit 1
fi
printf 'develop-refresh: actionlint %s\n' "$latest_actionlint"

printf 'develop-refresh: checking latest immutable GitHub Action releases\n'
python3 - <<'PY'
from pathlib import Path
import re
import subprocess
import sys

action_pattern = re.compile(
    r"uses:\s*([^\s@]+/[^\s@]+)@([0-9a-f]{40})\s*#\s*(v?\d+(?:\.\d+){2})"
)
actions: dict[str, tuple[str, str]] = {}
errors: list[str] = []

for workflow in sorted(Path(".github/workflows").glob("*.yml")):
    for line_number, line in enumerate(workflow.read_text().splitlines(), 1):
        if "uses:" not in line or line.lstrip().startswith("#"):
            continue
        match = action_pattern.search(line)
        if not match:
            errors.append(
                f"{workflow}:{line_number}: external action must use a 40-character SHA "
                "and an exact release comment"
            )
            continue
        repository, revision, version = match.groups()
        previous = actions.get(repository)
        if previous is not None and previous != (revision, version):
            errors.append(f"{repository}: workflow references are not consistent")
        actions[repository] = (revision, version)

for repository, (revision, version) in sorted(actions.items()):
    remote = f"https://github.com/{repository}.git"
    if repository == "dtolnay/rust-toolchain":
        output = subprocess.check_output(
            ["git", "ls-remote", remote, f"refs/heads/{version}"], text=True
        ).strip()
        branch_revision = output.split()[0] if output else ""
        if branch_revision != revision:
            errors.append(
                f"{repository}: {version} resolves to {branch_revision or 'nothing'}, "
                f"not pinned {revision}"
            )
        continue

    output = subprocess.check_output(
        ["git", "ls-remote", "--tags", remote], text=True
    )
    tags: dict[str, str] = {}
    for row in output.splitlines():
        tag_revision, ref = row.split("\t", 1)
        match = re.fullmatch(
            r"refs/tags/(v(\d+)\.(\d+)\.(\d+))(\^\{\})?", ref
        )
        if match is None:
            continue
        tag = match.group(1)
        if match.group(5) or tag not in tags:
            tags[tag] = tag_revision
    if not tags:
        errors.append(f"{repository}: no stable semantic-version tag found")
        continue
    latest = max(tags, key=lambda tag: tuple(map(int, tag[1:].split("."))))
    if version != latest:
        errors.append(f"{repository}: workflow uses {version}; latest is {latest}")
    elif tags[latest] != revision:
        errors.append(
            f"{repository}: {latest} resolves to {tags[latest]}, not pinned {revision}"
        )

if errors:
    print("develop-refresh: GitHub Action refresh required:", file=sys.stderr)
    for error in errors:
        print(f"  - {error}", file=sys.stderr)
    sys.exit(1)
print(f"develop-refresh: {len(actions)} GitHub Actions use their latest stable immutable pins")
PY

run "Cargo manifest requirements" \
  cargo upgrade --incompatible --exclude sse-stream
run "Cargo lockfile" cargo update
run "Cargo direct dependency freshness" \
  cargo outdated --root-deps-only --ignore sse-stream --exit-code 1

run "chat UI dependencies" \
  bun update --latest --cwd="$repo_root/examples/chat-ui"

printf '%s\n' \
  'develop-refresh: managed tools and dependencies are at the latest stable versions.' \
  'develop-refresh: sse-stream remains below 0.3 until rmcp public transport types are compatible.'
