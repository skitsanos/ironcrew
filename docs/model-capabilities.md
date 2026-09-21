# Model capability policy

IronCrew uses one offline policy for built-in provider request builders and Lua
construction checks. The default model remains `gpt-5.6-luna`. Existing keys are
reused; capability checks do not use credentials or make provider requests.

## Defaults and explicit choices

| Official OpenAI Luna request | Omitted effort | Output-token field |
| --- | --- | --- |
| Chat Completions, no function tools | `low` | `max_completion_tokens` |
| Chat Completions, function tools | `none` | `max_completion_tokens` |
| Responses, with or without tools | `low` | `max_output_tokens` |

Luna accepts `none`, `low`, `medium`, `high`, `xhigh`, and `max`, not `minimal`.
An explicit incompatible effort is rejected, never downgraded. On Responses,
agent effort overrides crew effort; omission uses the policy default. Crew-level
`reasoning_effort` is a Responses option. For Chat Completions, configure the
agent instead. Setting that crew option on another adapter now fails explicitly.

For Luna, omit `temperature` or set the provider default `1`. Other values fail
locally. The catalog also recognizes GPT-5.6 Sol, Terra, and the `gpt-5.6` alias
for effort validation, without changing their omitted-effort provider defaults.
Other model IDs retain provider-checked effort and temperature combinations.
All official Chat Completions requests use the current `max_completion_tokens`
field, independently of model spelling; custom endpoints retain `max_tokens`.

The Anthropic adapter exposes manual `thinking_budget`, not native
`output_config.effort`. It rejects per-agent `reasoning_effort`, non-default
temperature during manual thinking, forced JSON-schema tools with thinking,
and invalid budget/output-token combinations.
Known adaptive-only Claude aliases reject manual budgets with a clear error;
adaptive thinking is not implemented by this change. Supported explicit
temperature is preserved rather than silently discarded.

## Where checks run

- `Crew.new`: effective provider configuration and preloaded agents.
- `crew:add_agent`: effective agent model (including its override and the
  task-execution model route), declared tools, effort, and temperature.
- Conversation/dialog construction: effective session model and registered tools.
- Every built-in provider call: final resolved model and actual tools, including
  task overrides, tool rounds, streaming, and direct Rust callers.

`Agent.new` alone cannot know its future provider; it checks option syntax.
Custom Rust providers can implement `LlmProvider::validate_request`; its default
is a no-op, and the metrics wrapper forwards it. The policy revision is included
in built-in provider execution fingerprints, preventing silent reuse of a
persistent conversation under changed defaults.

`ironcrew validate --evaluate` runs these checks during bounded, effect-free
construction. Plain `validate` only compiles the entrypoint and checks separate
declarations. See [construction validation](cli.md#validate) for exit statuses
and the limits of evaluating dynamic Lua.

## Unknown models and custom endpoints

Official OpenAI model rules apply only to HTTPS `api.openai.com`, port 443, with
the root or `/v1` base path. Similar hostnames, other ports, and proxy URLs do
not inherit those rules just because a model name contains `gpt-5` or `luna`.
OpenAI aliases match exactly, or with a valid `YYYY-MM-DD` snapshot suffix;
Anthropic uses its compact `YYYYMMDD` snapshot format. An arbitrary
suffix or unknown ID is not treated as a known family or proof of availability.

A custom `base_url` explicitly selects a provider-compatible wire contract:
Chat Completions uses `max_tokens`, Responses uses `max_output_tokens`; explicit
effort is forwarded and omitted effort remains omitted. No Luna defaults or
model-specific restrictions are injected. Unknown official model IDs follow
the same provider-checked model policy, but use the official transport's
`max_completion_tokens` field on Chat Completions. Generic syntax and numeric
bounds still apply. A proxy requiring different semantics needs a custom Rust adapter or a
reviewed policy entry; there is no automatic model substitution.

## Sources and evidence boundary

Reviewed on **2026-09-20**:

- [OpenAI Luna model documentation](https://developers.openai.com/api/docs/models/gpt-5.6-luna)
  and [GPT-5.6 guide](https://developers.openai.com/api/docs/guides/latest-model?model=gpt-5.6):
  model identities and supported effort values. IronCrew deliberately chooses
  `low`, rather than relying on the API's omitted-effort default.
- [OpenAI Chat Completions reference](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create)
  and [Responses reasoning guide](https://developers.openai.com/api/docs/guides/reasoning#controlling-costs):
  transport-specific output-token fields.
- [Anthropic manual thinking](https://platform.claude.com/docs/en/build-with-claude/extended-thinking):
  manual budget constraints and adaptive-only migration boundary.
- [Anthropic API primer](https://platform.claude.com/docs/en/claude_api_primer):
  temperature and forced-tool restrictions with thinking.

The Luna temperature restriction retains the live-observed contract recorded in
[IC-009](issues/IC-009.md); the Chat Completions tool restriction retains the
pre-existing adapter guard and provider documentation. The current public model
page does not enumerate these two restrictions. They are fixture-backed here,
not newly live-verified. Policy
coverage does not establish availability, output quality, billing, or future
provider compatibility. Opt-in live smoke is tracked by [IC-048](issues/IC-048.md).
