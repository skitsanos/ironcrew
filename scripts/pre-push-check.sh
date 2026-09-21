#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

# Git exports repository-local variables to hooks. They override `cwd` for
# nested Git commands, so temporary-repository tests would otherwise operate on
# this worktree. The gate is already anchored at the repository root and can
# safely rediscover the worktree from `.git` after clearing them.
while IFS= read -r git_environment_name; do
  unset "$git_environment_name"
done < <(git rev-parse --local-env-vars)

require_command() {
  local command_name=$1
  local install_hint=$2

  if ! command -v "$command_name" >/dev/null 2>&1; then
    printf 'pre-push: required command not found: %s\n' "$command_name" >&2
    printf 'pre-push: %s\n' "$install_hint" >&2
    exit 1
  fi
}

require_exact_version() {
  local label=$1
  local expected=$2
  local actual=$3

  if [[ "$actual" != "$expected" ]]; then
    printf 'pre-push: %s version mismatch: expected %s, found %s\n' \
      "$label" "$expected" "$actual" >&2
    exit 1
  fi
}

run() {
  printf 'pre-push: running %s\n' "$1"
  shift
  "$@"
}

require_command git "Install Git before running the IronCrew validation gate."
require_command python3 "Install Python 3 before running the IronCrew validation gate."
require_command bun "Install Bun before running the IronCrew validation gate."
require_command cargo "Install the repository Rust toolchain before running the gate."
require_command rustc "Install the repository Rust toolchain before running the gate."
require_command rustup "Install rustup before running the IronCrew validation gate."
require_command actionlint "Install actionlint before running the gate."

if [[ -n "$(git status --porcelain=v1 --untracked-files=all)" ]]; then
  printf '%s\n' \
    'pre-push: commit or otherwise reconcile every tracked and untracked change before running the gate.' >&2
  git status --short >&2
  exit 1
fi

run "latest-stable tool and dependency refresh" \
  ./scripts/refresh-develop-dependencies.sh
if [[ -n "$(git status --porcelain=v1 --untracked-files=all)" ]]; then
  printf '%s\n' \
    'pre-push: the latest-stable refresh changed repository files.' \
    'pre-push: review and commit the refresh, then rerun the complete gate.' >&2
  git status --short >&2
  exit 1
fi

expected_rust=$(awk -F'"' '/^channel = / { print $2; exit }' rust-toolchain.toml)
expected_audit=$(
  awk -F'--version ' '/cargo install cargo-audit --version / { split($2, fields, " "); print fields[1]; exit }' \
    .github/workflows/ci.yml
)
expected_actionlint=$(
  awk -F'@v' '/actionlint\/cmd\/actionlint@v/ { print $2; exit }' .github/workflows/ci.yml
)

require_exact_version "Rust" "$expected_rust" "$(rustc --version | awk '{print $2}')"
require_exact_version "Cargo" "$expected_rust" "$(cargo --version | awk '{print $2}')"
require_exact_version "cargo-audit" "$expected_audit" "$(cargo audit --version | awk '{print $2}')"
require_exact_version "actionlint" "$expected_actionlint" "$(actionlint -version | head -n 1)"
printf 'pre-push: Bun %s (%s)\n' "$(bun --version)" "$(bun --revision)"

available_kib=$(LC_ALL=C df -Pk "$repo_root" | awk 'END {print $4}')
minimum_kib=$((4 * 1024 * 1024))
if [[ ! "$available_kib" =~ ^[0-9]+$ ]] || ((available_kib < minimum_kib)); then
  printf '%s\n' \
    'pre-push: at least 4 GiB of free disk is required before starting the full gate.' \
    'pre-push: remove only positively identified, rebuildable artifacts and retry.' >&2
  exit 1
fi

# macOS debug symbols and incremental caches can make one all-target run consume
# many gigabytes without changing which cfg paths, assertions, tests, or lints
# execute. Keep the local gate bounded while preserving the CI command contract.
export CARGO_INCREMENTAL=0
export CARGO_PROFILE_DEV_DEBUG=0

scratch_root=$(mktemp -d "${TMPDIR:-/tmp}/ironcrew-pre-push.XXXXXX")
cleanup() {
  rm -rf -- "$scratch_root"
}
trap cleanup EXIT

git status --porcelain=v1 -z --untracked-files=all >"$scratch_root/worktree-before"

run "Rust module-size policy" python3 -B scripts/check_module_size.py
run "repository Python tests" \
  python3 -B -m unittest discover -s scripts/tests -p 'test_*.py'
run "repository skill validation" bun run scripts/validate_skills.ts
run "issue-registry validation" bun run scripts/issues_registry.ts check
run "repository policy tests" bun test scripts/tests/*.test.ts
run "workflow lint" actionlint .github/workflows/*.yml
run "worktree whitespace validation" bun run scripts/check_worktree.ts
run "chat UI frozen install" \
  bun install --frozen-lockfile --cwd="$repo_root/examples/chat-ui"
run "chat UI production bundle" \
  bun build "$repo_root/examples/chat-ui/src/index.html" \
    --outdir "$scratch_root/chat-ui"

run "Rust formatting" cargo fmt --all -- --check
run "Rust no-default-features build" cargo build --no-default-features
run "Rust all-target Clippy" cargo clippy --all-targets -- -D warnings
run "Rust all-target tests" cargo test --all-targets
run "Rust documentation tests" cargo test --doc
run "dependency security audit" cargo audit --deny warnings

run "debug CLI build" cargo build --locked --bin ironcrew
run "Lua examples and offline runtime probes" \
  env IRONCREW_BIN="$repo_root/target/debug/ironcrew" ./scripts/check-lua-examples.sh
run "crew-effectiveness unit tests" \
  python3 -m unittest discover -s evaluations/crew-effectiveness -p 'test_*.py'
run "crew-effectiveness contract" \
  python3 evaluations/crew-effectiveness/evaluate.py \
    --mode contract \
    --binary target/debug/ironcrew \
    --report "$scratch_root/crew-effectiveness-contract.json"
run "replica-soak unit tests" \
  python3 -m unittest discover -s evaluations/replica-soak -p 'test_*.py'
run "replica-lifecycle unit tests" \
  python3 -B -m unittest discover -s evaluations/replica-lifecycle -p 'test_*.py'
run "live-smoke offline controls and CLI/HTTP contract" \
  env IRONCREW_SMOKE_TEST_BIN="$repo_root/target/debug/ironcrew" \
    python3 -B -m unittest discover -s evaluations/live-smoke -p 'test_*.py'

if [[ -n "${IRONCREW_TEST_PG_URL:-}" ]]; then
  run "PostgreSQL integration tests" \
    cargo test --locked --all-features \
      --test postgres_store_test \
      --test usage_storage_test \
      --test multi_replica_http_test \
      --test two_process_replica_acceptance_test \
      -- --test-threads=1
  printf 'pre-push: running PostgreSQL replica-soak contract\n'
  DATABASE_URL="$IRONCREW_TEST_PG_URL" \
    python3 evaluations/replica-soak/soak.py \
      --binary target/debug/ironcrew \
      --runs 2 \
      --duration-seconds 30 \
      --concurrency 1 \
      --report "$scratch_root/replica-soak/result.json"
else
  printf '%s\n' \
    'pre-push: PostgreSQL integration not run because IRONCREW_TEST_PG_URL is unset.' \
    'pre-push: storage, HITL, journal, lease, or replica changes require a disposable latest-stable PostgreSQL database before push.'
fi

run "locked release build" cargo build --release --locked

git status --porcelain=v1 -z --untracked-files=all >"$scratch_root/worktree-after"
if ! cmp -s "$scratch_root/worktree-before" "$scratch_root/worktree-after"; then
  printf '%s\n' \
    'pre-push: validation changed the worktree; inspect and restore or retain those changes deliberately.' >&2
  git status --short >&2
  exit 1
fi

printf '%s\n' \
  'pre-push: every locally reproducible CI gate passed.' \
  'pre-push: GitHub CI remains authoritative for macOS, Windows, and protected service environments.'
