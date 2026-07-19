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
through IronCrew's own JSON Schema validator using `report-v1.schema.json`.

## Opt-in live exploratory run

The live path uses the provider configuration available to the IronCrew
process. From the repository root, IronCrew loads the existing `.env`; the
evaluator neither parses nor prints the API key.

```bash
python3 evaluations/crew-effectiveness/evaluate.py \
  --mode live \
  --binary target/debug/ironcrew \
  --model gpt-4.1-mini \
  --repetitions 2 \
  --report evaluations/crew-effectiveness/reports/live-pilot.json
```

Six cases with two repetitions produce 36 CLI runs and 96 planned model calls.
Variant order is deterministically shuffled to reduce ordering bias. The model
name, binary hash, Git revision/dirty state, dataset hashes, run order seed,
and all raw scored outputs are retained in the report.

This is intentionally described as exploratory evidence. The corpus is too
small to establish broad superiority, temperature zero is not a model seed,
and provider/network variance still affects latency.

## Metrics

The primary quality metric is **grounded correctness**: an exact correct option
ID plus one complete acceptable citation set. Reports also keep these dimensions
separate:

- answer correctness
- citation precision, recall, and F1
- execution, JSON parse, and output-schema success
- run latency median and p95
- median and total provider-reported tokens
- paired grounded-correctness wins, ties, losses, and mean delta versus single

No LLM judge is used. Token counts come from provider usage fields surfaced by
IronCrew; contract-mode token values are deterministic synthetic fixture data.

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
