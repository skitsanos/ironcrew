# Crew Effectiveness Evaluation

This evaluator compares three IronCrew orchestration shapes on the same small,
synthetic, evidence-grounded decision set:

- `single`: one structured final-agent call
- `dag`: parallel extraction and challenge tasks, then the same final agent
- `collaborative`: a two-agent discussion, then the same final agent

The source packets and scoring oracle are separate JSONL files. Every source
question exposes the same explicit single-select options to every variant,
using stable lowercase option IDs and human-readable labels. Only
`cases.v1.jsonl` is injected into the model prompt; `oracle.v1.jsonl` keeps the
correct option ID or IDs and acceptable citation sets hidden from live models.
In contract mode only, the explicitly synthetic mock provider reads the oracle
to return known-good fixture answers. All variants use the same final system
prompt, model, temperature, token cap, evidence packet, options, and JSON
Schema.

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

Contract mode starts the local standard-library `mock_openai.py` server and
runs all six cases through all three variants using the real `ironcrew run
--json` path. It covers 18 CLI runs and 48 mock model requests.

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
through IronCrew's own JSON Schema validator using `report-v2.schema.json`. The
v1 schema remains tracked for historical receipts.

## Predeclared execution and selection plan

`decision-plan.v1.json` is validated strictly before IronCrew or a provider is
started. It caps selected case bytes, CLI runs, planned model calls, and the
sum of the Lua flows' requested maximum output tokens. Inspect the exact
workload without requiring a built binary or writing a report:

```bash
python3 evaluations/crew-effectiveness/evaluate.py \
  --mode live \
  --provider-id openai-api \
  --repetitions 5 \
  --dry-run-plan
```

The checked-in plan admits at most 90 CLI runs, 240 planned model calls, and
147,000 requested maximum output tokens. The plan also pins the SHA-256 of
`crew.lua` and its per-variant call/token accounting, so an unreviewed flow
change fails before provider execution. These are workload ceilings, not a
currency estimate: provider pricing and actual usage can change independently.
Increasing repetitions, cases, prompt bytes, calls, or token caps requires an
explicitly reviewed plan change before any provider request can run.

## Opt-in live exploratory run

The live path uses the provider configuration available to the IronCrew
process. From the repository root, IronCrew loads the existing `.env`; the
evaluator neither parses nor prints the API key.

```bash
python3 evaluations/crew-effectiveness/evaluate.py \
  --mode live \
  --binary target/debug/ironcrew \
  --provider-id openai-api \
  --model gpt-4.1-mini \
  --repetitions 5 \
  --report evaluations/crew-effectiveness/reports/live-pilot.json
```

Six cases with five repetitions produce the plan maximum of 90 CLI runs and
240 planned model calls. Live mode requires both the repetition count and a
non-secret, operator-declared provider identity; it will not silently use a
non-decision-grade default. Run this paid path only after approving the
provider, model, and plan shown by `--dry-run-plan`. Variant order is deterministically
shuffled to reduce ordering bias. The model name, binary hash, Git
revision/dirty state, dataset and plan hashes, run order seed, and all raw
scored outputs are retained in the report.

This is intentionally described as exploratory evidence. The corpus is too
small to establish broad superiority, temperature zero is not a model seed,
and provider/network variance still affects latency. The report therefore
records a deterministic bootstrap interval over independent case-level mean
deltas. The two topology comparisons use a Bonferroni-adjusted per-comparison
confidence level to preserve the plan's family-wise confidence target. Each
topology is evaluated against predeclared unique-case, paired-sample, success,
quality, confidence, token, and latency thresholds. Its decision is one of `insufficient_evidence`,
`single_preferred`, or `crew_qualified`; contract mode always reports
`not_applicable`. A qualified result applies only to this dataset and model.

## Metrics

The primary quality metric is **grounded correctness**: an exact correct option
ID plus one complete acceptable citation set. Reports also keep these dimensions
separate:

- answer correctness
- citation precision, recall, and F1
- execution, JSON parse, and output-schema success
- run latency median and p95
- median and total provider-reported tokens
- paired grounded-correctness wins, ties, losses, mean delta, and deterministic
  percentile-bootstrap interval versus single
- matched-pair mean token and latency multipliers versus single
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

1. Add source-only evidence and questions to `cases.v1.jsonl`.
2. Give every question at least three options with unique, stable lowercase
   snake-case IDs and unique non-empty labels. Include plausible distractors.
3. Include an `insufficient_evidence` option whenever the evidence may not
   support a unique selection.
4. Add matching `correct_option_ids` and citation sets to `oracle.v1.jsonl`.
   Every correct ID must exist in that source question's options.
5. Keep case IDs, evidence IDs, question IDs, and per-question option IDs
   unique.
6. Never put correct-option IDs, citation sets, correctness flags, or other
   oracle material in a source packet. Option objects contain only `id` and
   `label`.
7. Prefer objectively scoreable questions and review distractor quality and
   option order before collecting live evidence.

Generated reports are ignored under `reports/`. Promote a reviewed report to a
tracked evidence artifact only deliberately, with its limitations intact.
