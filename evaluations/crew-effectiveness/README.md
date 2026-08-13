# Crew Effectiveness Evaluation

This evaluator compares three IronCrew orchestration shapes on the same
12-case, evidence-grounded decision set:

- `single`: one structured final-agent call
- `dag`: parallel extraction and challenge tasks, then the same final agent
- `collaborative`: a two-agent discussion, then the same final agent

The corpus is six base synthetic cases plus two three-case representative
synthetic intended-use packs: `software-delivery` and `security-operations`.
These packs model operational decisions; they are not sampled production
records, provider outputs, or evidence of performance on real customer data.
Each pack has a versioned manifest, separate source packets and oracle, file
hashes, derivation and oracle-method boundaries, and an independent-review
receipt. Only source packets are injected into model prompts. Correct option
IDs and acceptable citation sets remain in the hidden oracle; pack membership
also stays outside the prompt.

Every source question exposes the same explicit single-select options to every
variant, using stable lowercase option IDs and human-readable labels. In
contract mode only, the explicitly synthetic mock provider reads the oracle to
return known-good fixture answers. All variants use the same final-agent system
prompt, model, omitted (provider-default) temperature and reasoning effort,
final-agent token cap, evidence packet, options, and JSON Schema.

## Deterministic contract smoke

Build IronCrew, then run:

```bash
cargo build --bin ironcrew
python3 -m unittest discover \
  -s evaluations/crew-effectiveness \
  -p 'test_*.py'
python3 evaluations/crew-effectiveness/evaluate.py \
  --mode contract \
  --binary target/debug/ironcrew \
  --report evaluations/crew-effectiveness/reports/contract.json
```

Contract mode auto-discovers both checked-in domain-pack manifests, starts the
local standard-library `mock_openai.py` server, and runs all 12 cases through
all three variants using the real `ironcrew run --json` path. It covers 36 CLI
runs, 96 mock model requests, and 72 grounded fixture answers.

The mock reads the oracle so it can produce a deterministic known-good answer.
Accordingly, every contract report contains:

```json
{
  "mode": "contract",
  "effectiveness_evidence": false
}
```

Contract scores prove only that CLI execution, Lua orchestration, run-record
persistence, token accounting, structured output parsing, and scoring remain
wired together. They must never be cited as evidence that crews outperform a
single agent. Before succeeding, `evaluate.py` also runs the generated document
through IronCrew's own JSON Schema validator using `report-v3.schema.json`.
Schema and report documents are each limited to 2 MiB and cross the process
boundary through bounded modules rather than command-line arguments.
The v1 and v2 schemas and dated GPT-4.1 receipts remain unchanged historical
evidence; they are not rewritten into the current contract.

## Predeclared execution and selection plan

`decision-plan.v2.json` is validated strictly before IronCrew or a provider is
started. It pins the Lua flow hash and the reviewed corpus shape, repetitions,
per-variant calls and output tokens, total and single-case input bytes, input
tokens, and selection thresholds. Inspect the exact workload without requiring
a built binary or writing a report:

```bash
python3 evaluations/crew-effectiveness/evaluate.py \
  --mode live \
  --provider-id openai-api \
  --model gpt-5.6-luna \
  --domain-pack-manifest evaluations/crew-effectiveness/domain-packs/software-delivery.v1/manifest.v1.json \
  --domain-pack-manifest evaluations/crew-effectiveness/domain-packs/security-operations.v1/manifest.v1.json \
  --plan evaluations/crew-effectiveness/decision-plan.v2.json \
  --repetitions 5 \
  --dry-run-plan
```

The exact 12-case, five-repetition plan admits 180 CLI runs and 480 planned
model calls, with a 9,600,000-token aggregate costing allowance, a 20,000-token
costing allowance per request, an exact 18,000-byte serialized request-body
limit before network access, and 294,000 requested output tokens. It prices
every input token at the more conservative 1.25x cache-write rate, producing a
$2.7528 planned upper estimate under a $3 approval budget. The token allowance
is for conservative costing, not an exact tokenizer or pre-send token cap; the
byte limit is the independently enforced runtime boundary. Neither estimate is
an invoice.

The plan also pins the exact ordered frozen corpus identity: aggregate SHA-256
`bb73ad0d4835a407e22bc35de1562a9f600e33583ec219e40eba2b7b4b0c45cf`
plus every pack version, case count, manifest hash, cases hash, and oracle hash.
A missing, reordered, renamed, or changed pack is rejected before binary or
credential checks. Live mode likewise accepts only provider ID `openai-api`
and model ID `gpt-5.6-luna`; a syntactically valid alternative fails before any
provider setup or execution.

The [official GPT-5.6 Luna model page](https://developers.openai.com/api/docs/models/gpt-5.6-luna),
refreshed on 2026-08-12, listed $0.20 per million input tokens, $0.02 per
million cached input tokens, and $1.20 per million output tokens. It also says
cache writes cost 1.25x the uncached input rate, and requests with more than
272,000 input tokens cost 2x input and 1.5x output for the full request. The
20,000-token per-request costing allowance remains below that long-context
boundary; the aggregate 9.6 million allowance does not trigger a per-request
surcharge. IronCrew also enforces a 3,200 ms provider-start interval within a
CLI process, while the evaluator leaves at least the same gap after each CLI
process finishes before starting the next one. This bounds the reviewed rolling
window to 19 starts and a 395,200-token allowance per 60 seconds.
Reconfirm the direct official page immediately before a paid run because
pricing and account rate limits can change independently. Increasing cases,
repetitions, prompt bytes or tokens, calls, or output caps requires an
explicitly reviewed plan change before any provider request can run.

## Opt-in bounded live run

The live path accepts `OPENAI_API_KEY` and optional `OPENAI_BASE_URL` from the
process environment or the ignored root `.env`, without printing or retaining
their values. It rejects ambiguous duplicates and passes a minimal child
environment. IC-009 requires the official OpenAI API base URL. The model stays
explicit even though `gpt-5.6-luna` is the active default, so the reviewed
invocation and receipt are unambiguous.

Build and validate the release binary before the paid run, then inspect the
provider-free plan:

```bash
cargo build --locked --release --bin ironcrew
python3 -m unittest discover \
  -s evaluations/crew-effectiveness \
  -p 'test_*.py'
python3 evaluations/crew-effectiveness/evaluate.py \
  --mode live \
  --binary target/release/ironcrew \
  --provider-id openai-api \
  --model gpt-5.6-luna \
  --domain-pack-manifest evaluations/crew-effectiveness/domain-packs/software-delivery.v1/manifest.v1.json \
  --domain-pack-manifest evaluations/crew-effectiveness/domain-packs/security-operations.v1/manifest.v1.json \
  --plan evaluations/crew-effectiveness/decision-plan.v2.json \
  --repetitions 5 \
  --timeout-seconds 600 \
  --progress-every 10 \
  --dry-run-plan
```

Only after reviewing that output and rechecking current pricing and account
rate limits, remove `--dry-run-plan` and add the ignored working receipt:

```bash
python3 evaluations/crew-effectiveness/evaluate.py \
  --mode live \
  --binary target/release/ironcrew \
  --provider-id openai-api \
  --model gpt-5.6-luna \
  --domain-pack-manifest evaluations/crew-effectiveness/domain-packs/software-delivery.v1/manifest.v1.json \
  --domain-pack-manifest evaluations/crew-effectiveness/domain-packs/security-operations.v1/manifest.v1.json \
  --plan evaluations/crew-effectiveness/decision-plan.v2.json \
  --repetitions 5 \
  --timeout-seconds 600 \
  --progress-every 10 \
  --report evaluations/crew-effectiveness/reports/ic009-luna-live-working.json
```

This command is an approved shape, not evidence that the live run happened.
Live mode requires the complete 12-case corpus, exactly five repetitions, and
a non-secret operator-declared provider identity. Variant order is
deterministically shuffled to reduce ordering bias. Progress goes to stderr and
does not replace the final JSON receipt.

Report v3 retains repository-relative binary identity and hash; start/end HEAD,
tracked-diff, and non-ignored-untracked source manifests; aggregate and per-pack
dataset identities; prompt, completion, cached, and total token usage; a
conservative per-task and aggregate cost estimate; per-pack summaries; run
failures and structured-output validity; latency; randomized-order seed; and
the predeclared topology decision. Any source or binary change during execution
fails the run. Failed or partial provider runs keep usage and cost `null` when
complete accounting is unavailable, rather than misreporting a partial lower
bound as full cost.

Every successful task must provide positive prompt, completion, and total usage.
The evaluator checks usage against the flow's exact task call multiplicity and
output budget: `discussion` aggregates three calls with a 60,000-token costing
allowance and 1,500 completion-token limit; all other tasks are one call, with
500-token limits for `extract`/`challenge` and 800 for `final`. A missing,
malformed, zero, or over-allowance successful-task measurement stops the
remaining paid workload and preserves the partial receipt. Report-schema
validation runs in an isolated temporary directory with no provider credential.

This is intentionally bounded local live-provider evidence. The representative
packs are synthetic, one provider/model configuration cannot establish model
generality, provider-default parameters are not a model seed, and
provider/network variance still affects latency. The report therefore records
a deterministic bootstrap interval over independent case-level mean deltas.
The two topology comparisons use a Bonferroni-adjusted per-comparison
confidence level to preserve the plan's family-wise confidence target. Each
topology is evaluated against predeclared unique-case, paired-sample, success,
quality, confidence, token, and latency thresholds. Its decision is one of `insufficient_evidence`,
`single_preferred`, or `crew_qualified`; contract mode always reports
`not_applicable`. A qualified result applies only to this dataset and model.

## Retained GPT-5.6 Luna result — 2026-08-12

The reviewed plan completed all 180/180 local live-provider runs with no
execution, parse, or output-schema failures and with complete token, latency,
and cost-estimate coverage. It covered all 12 cases five times in each
topology, producing 60 matched comparisons per crew candidate. Source and
release-binary identities stayed unchanged throughout the run.

| Variant | Grounded correctness | Total tokens | Median latency | p95 latency |
| --- | ---: | ---: | ---: | ---: |
| `single` | 0.6500 | 43,081 | 1,959 ms | 2,926 ms |
| `dag` | 0.7833 | 138,909 | 8,840.5 ms | 11,246 ms |
| `collaborative` | 0.7333 | 215,647 | 13,126 ms | 16,045 ms |

Against `single`, the DAG recorded 18 wins, 40 ties, and 2 losses. Its +0.1333
mean grounded-correctness delta had a Bonferroni-adjusted 97.5% interval of
[0.0500, 0.2167], while its mean token and latency multipliers were 3.2113x
and 4.5645x. It met every predeclared check. Collaborative recorded 18 wins,
34 ties, and 8 losses, but its +0.0833 delta interval [-0.0417, 0.2083]
crossed zero and its 6.7050x mean latency multiplier exceeded the 6x ceiling.

The resulting status is `crew_qualified` with `dag` recommended. This means at
least one candidate crew topology met every threshold for this frozen plan; it
does not mean collaborative qualified, that all crews beat simpler workflows,
or that the result generalizes beyond this corpus and model.

Provider-reported totals were 289,816 prompt, 107,821 completion, zero cached,
and 397,637 total tokens. Applying the frozen pricing contract conservatively
gave a $0.2018392 observed estimated upper bound, below the $2.7528 planned
bound and $3 approval budget. The estimate is not an invoice.

See the retained
[`2026-08-12-gpt-5.6-luna-12-case-5x.json`](reports/2026-08-12-gpt-5.6-luna-12-case-5x.json)
receipt and its
[`2026-08-12-gpt-5.6-luna-12-case-5x.md`](reports/2026-08-12-gpt-5.6-luna-12-case-5x.md)
interpretation. They bind the exact synthetic corpus, plan, flow, source
manifest, release binary, provider/model identity, raw scored outputs, usage,
and decision. The two intended-use packs are representative synthetic
scenarios, not production samples. This single local OpenAI API/GPT-5.6 Luna
configuration does not establish model/provider generality, deterministic
provider output, deployed-platform latency, or production performance. The
dated GPT-4.1 receipts remain unchanged historical evidence.

## Metrics

The primary quality metric is **grounded correctness**: an exact correct option
ID plus one complete acceptable citation set. Reports also keep these dimensions
separate:

- answer correctness
- citation precision, recall, and F1
- execution, JSON parse, and output-schema success
- run latency median and p95
- prompt, completion, cached, and total provider-reported tokens
- conservative token-derived cost estimates and coverage
- paired grounded-correctness wins, ties, losses, mean delta, and deterministic
  percentile-bootstrap interval versus single
- matched-pair mean token and latency multipliers versus single
- per-pack variant summaries
- predeclared topology checks and the resulting bounded recommendation

No LLM judge is used. Token counts come from provider usage fields surfaced by
IronCrew; contract-mode token values are deterministic synthetic fixture data.
If a paid CLI process exits, times out, or returns a non-success run status,
aggregate token usage is recorded as unknown (`null`), never as zero or as a
potentially partial lower bound.

### Exact option-ID scoring

For each question, the final model output must set `answer` to exactly one
source-visible option `id`. Correctness is strict, case-sensitive equality
against the hidden oracle's `correct_option_ids`; labels, prose, surrounding
whitespace, changed case, and invented IDs are rejected by output validation.
There is no normalization, substring matching, or semantic judge. This removes
free-text paraphrase coverage from the scorer and applies the same answer
contract to `single`, `dag`, and `collaborative` runs.

Every question has plausible distractors and an `insufficient_evidence` option
where abstention may be warranted. Option objects contain only `id` and
`label`; correctness flags and citation requirements remain oracle-only. This
design reduces scorer ambiguity, but it does not eliminate benchmark-design
risk: weak distractors, option-order effects, guessing, or options that encode
the expected answer too obviously can still distort results. Review those
properties before treating a live report as comparative evidence.

## Dataset extension rules

1. Add source-only evidence and questions to the base JSONL files, or create a
   versioned domain pack with a manifest, cases, and oracle.
2. Give every question at least three options with unique, stable lowercase
   snake-case IDs and unique non-empty labels. Include plausible distractors.
3. Include an `insufficient_evidence` option whenever the evidence may not
   support a unique selection.
4. Add matching `correct_option_ids` and citation sets to `oracle.v1.jsonl`.
   Every correct ID must exist in that source question's options.
5. Keep case IDs globally unique across packs; keep evidence IDs, question IDs,
   and per-question option IDs unique within their defined scopes.
6. Never put correct-option IDs, citation sets, correctness flags, or other
   oracle material in a source packet. Option objects contain only `id` and
   `label`.
7. Record each pack's intended use, derivation/source boundary, oracle method,
   independent review, case count, and exact file hashes.
8. Prefer objectively scoreable questions and review distractor quality and
   option order before collecting live evidence.

Generated working reports, including `ic009-luna-live-working.json`, remain
ignored under `reports/`. Only the narrowly reviewed 2026-08-12 GPT-5.6 Luna
JSON/Markdown pair is retained as decision evidence, with its source, model,
pricing, and synthetic-corpus limitations intact. Do not overwrite or rewrite
the dated GPT-4.1 receipts.
