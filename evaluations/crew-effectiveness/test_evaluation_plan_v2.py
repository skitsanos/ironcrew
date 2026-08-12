from __future__ import annotations

import copy
import json
import tempfile
import unittest
from pathlib import Path

import evaluate
from corpus_loader import load_corpus
from evaluation_plan_v2 import load_plan, preflight


BASE_DIR = Path(__file__).resolve().parent


def corpus():
    return load_corpus(
        base_cases_path=BASE_DIR / "cases.v1.jsonl",
        base_oracle_path=BASE_DIR / "oracle.v1.jsonl",
        manifest_paths=sorted((BASE_DIR / "domain-packs").glob("*/manifest.v1.json")),
        validate_dataset=evaluate.validate_dataset,
    )


class EvaluationPlanV2Tests(unittest.TestCase):
    def test_complete_workload_matches_the_reviewed_plan(self) -> None:
        loaded = corpus()
        plan = load_plan(BASE_DIR / "decision-plan.v2.json", BASE_DIR)
        sizes = [
            len(json.dumps(case, sort_keys=True, separators=(",", ":")).encode())
            for case in loaded.cases
        ]
        receipt = preflight(
            plan=plan, corpus_receipt=loaded.receipt, case_sizes=sizes, repetitions=5
        )
        self.assertEqual(receipt["cli_runs"], 180)
        self.assertEqual(receipt["llm_calls"], 480)
        self.assertEqual(receipt["input_token_costing_allowance"], 9_600_000)
        self.assertEqual(receipt["maximum_output_tokens"], 294_000)
        self.assertEqual(receipt["planned_cost_upper_bound_usd"], 2.7528)
        self.assertEqual(
            plan["dataset"]["aggregate_sha256"],
            "bb73ad0d4835a407e22bc35de1562a9f600e33583ec219e40eba2b7b4b0c45cf",
        )
        self.assertEqual(
            [pack["pack_id"] for pack in plan["dataset"]["packs"]],
            ["synthetic-core-v1", "security-operations", "software-delivery"],
        )

    def test_underpowered_or_changed_flow_is_rejected(self) -> None:
        loaded = corpus()
        sizes = [len(json.dumps(case).encode()) for case in loaded.cases]
        plan = load_plan(BASE_DIR / "decision-plan.v2.json", BASE_DIR)
        with self.assertRaisesRegex(ValueError, "must equal reviewed"):
            preflight(
                plan=plan, corpus_receipt=loaded.receipt, case_sizes=sizes, repetitions=4
            )

        with tempfile.TemporaryDirectory() as temporary:
            changed = json.loads((BASE_DIR / "decision-plan.v2.json").read_text())
            changed["flow"]["sha256"] = "0" * 64
            path = Path(temporary) / "plan.json"
            path.write_text(json.dumps(changed), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "crew.lua SHA-256"):
                load_plan(path, BASE_DIR)

    def test_cost_and_input_caps_are_derived_not_independent(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            changed = json.loads((BASE_DIR / "decision-plan.v2.json").read_text())
            changed["limits"]["max_planned_input_tokens"] -= 1
            path = Path(temporary) / "plan.json"
            path.write_text(json.dumps(changed), encoding="utf-8")
            with self.assertRaisesRegex(
                ValueError, "calls times per-request costing allowance"
            ):
                load_plan(path, BASE_DIR)

    def test_frozen_corpus_identity_rejects_mutation_or_wrong_pack(self) -> None:
        loaded = corpus()
        plan = load_plan(BASE_DIR / "decision-plan.v2.json", BASE_DIR)
        sizes = [len(json.dumps(case).encode()) for case in loaded.cases]
        for mutate in (
            lambda receipt: receipt.__setitem__("aggregate_sha256", "0" * 64),
            lambda receipt: receipt["packs"][1].__setitem__("pack_id", "wrong-pack"),
        ):
            changed = copy.deepcopy(loaded.receipt)
            mutate(changed)
            with self.assertRaisesRegex(ValueError, "frozen dataset identity"):
                preflight(
                    plan=plan,
                    corpus_receipt=changed,
                    case_sizes=sizes,
                    repetitions=5,
                )


if __name__ == "__main__":
    unittest.main()
