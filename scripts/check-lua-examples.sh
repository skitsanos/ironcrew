#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

if [[ -n "${IRONCREW_BIN:-}" ]]; then
  ironcrew_bin=$IRONCREW_BIN
  test -x "$ironcrew_bin"
else
  ironcrew_bin=$repo_root/target/debug/ironcrew
  cargo build --bin ironcrew
fi

# Override any developer or deployment `.env` values. Runtime probes use only
# an isolated JSON store under `probe_root`, and the selected probe branches
# contain no provider or HTTP-tool execution path.
export IRONCREW_STORE=json
export IRONCREW_LOG=error
export OPENAI_API_KEY=offline-example-probe

validated=0
while IFS= read -r lua_file; do
  [[ -n "$lua_file" ]] || continue
  "$ironcrew_bin" validate "$lua_file" >/dev/null
  validated=$((validated + 1))
done < <(
  git ls-files -- 'examples/**/*.lua' | LC_ALL=C sort -u
)

probe_root=$(mktemp -d "${TMPDIR:-/tmp}/ironcrew-lua-probes.XXXXXX")
cleanup() {
  rm -rf -- "$probe_root"
}
trap cleanup EXIT

# Real, provider-free Lua execution: require(), run_flow(), input injection,
# output conversion, and module caching all execute through the production VM.
cp -R examples/shared-modules "$probe_root/shared-modules"
shared_output=$(
  "$ironcrew_bin" run "$probe_root/shared-modules" \
    --input '{"title":"CI runtime probe"}'
)
grep -Fq "Sub-flow report.lua" <<<"$shared_output"
grep -Fq "require is cached: same table = true" <<<"$shared_output"

# Execute the real ask-human flow setup without a provider call or prompt.
mkdir -p "$probe_root/ask-human"
cp examples/ask-human/crew.lua "$probe_root/ask-human/crew.lua"
ask_output=$(
  "$ironcrew_bin" run "$probe_root/ask-human" \
    --input '{"setup_only":true}'
)
grep -Fq "ask-human setup probe passed" <<<"$ask_output"

# Capture-mode execution verifies the HITL-specific contracts without calling
# a provider: both fixed questions are reached, and the agent-driven showcase
# exposes both tools with file_write behind a crew-level approval gate.
ask_graph=$(
  "$ironcrew_bin" graph examples/ask-human \
    --output "$probe_root/ask-human.html"
)
grep -Fq "1 agent(s), 1 task(s)" <<<"$ask_graph"
grep -Fq '"human_inputs": [' "$probe_root/ask-human.html"
grep -Fq 'What should the announcement be about?' "$probe_root/ask-human.html"
grep -Fq 'Publish this draft?' "$probe_root/ask-human.html"

approval_graph=$(
  "$ironcrew_bin" graph examples/human-approval \
    --output "$probe_root/human-approval.html"
)
grep -Fq "1 agent(s), 1 task(s)" <<<"$approval_graph"
grep -Fq '"require_approval": [' "$probe_root/human-approval.html"
grep -Fq '"ask_human"' "$probe_root/human-approval.html"
grep -Fq '"file_write"' "$probe_root/human-approval.html"

printf 'Lua examples: %d files validated; 4 offline probes passed.\n' "$validated"
