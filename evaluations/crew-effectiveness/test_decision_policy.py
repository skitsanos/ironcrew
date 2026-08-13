from __future__ import annotations

import copy
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import decision_policy


BASE_DIR = Path(__file__).resolve().parent

class PlanValidationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.plan = decision_policy.load_plan(BASE_DIR / "decision-plan.v1.json")

    def test_default_plan_admits_exact_five_repetition_workload(self) -> None:
        planned = decision_policy.preflight(
            plan=self.plan,
            case_input_bytes=8_000,
            case_count=6,
            repetitions=5,
            variants=("single", "dag", "collaborative"),
            calls_per_run={"single": 1, "dag": 3, "collaborative": 4},
            output_tokens_per_run={"single": 800, "dag": 1_800, "collaborative": 2_300},
        )
        self.assertEqual(
            planned,
            {
                "case_input_bytes": 8_000,
                "cli_runs": 90,
                "llm_calls": 240,
                "maximum_output_tokens": 147_000,
                "paired_comparisons_per_candidate": 30,
                "unique_cases": 6,
            },
        )

    def test_preflight_rejects_one_run_over_the_plan(self) -> None:
        with self.assertRaisesRegex(ValueError, "rejected before provider execution"):
            decision_policy.preflight(
                plan=self.plan,
                case_input_bytes=8_000,
                case_count=6,
                repetitions=6,
                variants=("single", "dag", "collaborative"),
                calls_per_run={"single": 1, "dag": 3, "collaborative": 4},
                output_tokens_per_run={
                    "single": 800,
                    "dag": 1_800,
                    "collaborative": 2_300,
                },
            )

    def test_live_preflight_rejects_an_underpowered_paid_run(self) -> None:
        with self.assertRaisesRegex(ValueError, "minimum_paired_count"):
            decision_policy.preflight(
                plan=self.plan,
                case_input_bytes=8_000,
                case_count=6,
                repetitions=1,
                variants=("single", "dag", "collaborative"),
                calls_per_run={"single": 1, "dag": 3, "collaborative": 4},
                output_tokens_per_run={
                    "single": 800,
                    "dag": 1_800,
                    "collaborative": 2_300,
                },
                require_decision_grade=True,
            )

    def test_plan_rejects_unknown_fields_and_nonfinite_values(self) -> None:
        unknown = copy.deepcopy(self.plan)
        unknown["limits"]["unreviewed_limit"] = 1
        with self.assertRaisesRegex(ValueError, "must contain exactly"):
            decision_policy.validate_plan(unknown)

        nonfinite = copy.deepcopy(self.plan)
        nonfinite["selection"]["maximum_mean_token_multiplier"] = float("inf")
        with self.assertRaisesRegex(ValueError, "must be a number"):
            decision_policy.validate_plan(nonfinite)

        wrong_comparisons = copy.deepcopy(self.plan)
        wrong_comparisons["uncertainty"]["comparison_count"] = 1
        with self.assertRaisesRegex(ValueError, "two crew comparisons"):
            decision_policy.validate_plan(wrong_comparisons)

        dishonest_budget = copy.deepcopy(self.plan)
        dishonest_budget["flow"]["variants"]["single"]["maximum_output_tokens"] = 801
        with self.assertRaisesRegex(ValueError, "reviewed crew.lua budget"):
            decision_policy.validate_plan(dishonest_budget)

    def test_dry_run_does_not_require_binary_or_write_report(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            report = Path(temporary) / "must-not-exist.json"
            completed = subprocess.run(
                [
                    sys.executable,
                    str(BASE_DIR / "evaluate.py"),
                    "--mode",
                    "live",
                    "--dry-run-plan",
                    "--repetitions",
                    "5",
                    "--provider-id",
                    "openai-api",
                    "--binary",
                    str(Path(temporary) / "missing-ironcrew"),
                    "--report",
                    str(report),
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            output = json.loads(completed.stdout)
            self.assertEqual(output["model"], "gpt-5.6-luna")
            self.assertEqual(output["provider_id"], "openai-api")
            self.assertEqual(output["planned_work"]["cli_runs"], 180)
            self.assertEqual(output["planned_work"]["llm_calls"], 480)
            self.assertEqual(output["planned_work"]["maximum_output_tokens"], 294_000)
            self.assertEqual(output["planned_work"]["planned_cost_upper_bound_usd"], 2.7528)
            self.assertFalse(report.exists())

            rejected = subprocess.run(
                [
                    sys.executable,
                    str(BASE_DIR / "evaluate.py"),
                    "--mode",
                    "live",
                    "--repetitions",
                    "6",
                    "--provider-id",
                    "openai-api",
                    "--binary",
                    str(Path(temporary) / "missing-ironcrew"),
                    "--report",
                    str(report),
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertNotEqual(rejected.returncode, 0)
            self.assertIn("rejected before provider execution", rejected.stderr)
            self.assertNotIn("binary is not executable", rejected.stderr)
            self.assertFalse(report.exists())

    def test_changed_flow_hash_is_rejected_before_binary_execution(self) -> None:
        altered = json.loads((BASE_DIR / "decision-plan.v2.json").read_text())
        altered["flow"]["sha256"] = "0" * 64
        with tempfile.TemporaryDirectory() as temporary:
            plan_path = Path(temporary) / "altered-plan.json"
            plan_path.write_text(json.dumps(altered), encoding="utf-8")
            completed = subprocess.run(
                [
                    sys.executable,
                    str(BASE_DIR / "evaluate.py"),
                    "--mode",
                    "live",
                    "--provider-id",
                    "openai-api",
                    "--repetitions",
                    "5",
                    "--plan",
                    str(plan_path),
                    "--binary",
                    str(Path(temporary) / "missing-ironcrew"),
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertNotEqual(completed.returncode, 0)
            self.assertIn("SHA-256 does not match", completed.stderr)
            self.assertNotIn("binary is not executable", completed.stderr)

    def test_live_mode_requires_explicit_repetitions_and_provider_identity(self) -> None:
        base = [sys.executable, str(BASE_DIR / "evaluate.py"), "--mode", "live"]
        missing_repetitions = subprocess.run(
            [*base, "--provider-id", "openai-api", "--dry-run-plan"],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertIn("--repetitions is required", missing_repetitions.stderr)

        missing_provider = subprocess.run(
            [*base, "--repetitions", "5", "--dry-run-plan"],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertIn("--provider-id is required", missing_provider.stderr)

        invalid_model = subprocess.run(
            [
                sys.executable,
                str(BASE_DIR / "evaluate.py"),
                "--mode",
                "contract",
                "--model",
                "",
                "--dry-run-plan",
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertIn("--model must be a non-empty", invalid_model.stderr)

    def test_paid_run_rejects_wrong_model_or_provider_before_binary_lookup(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            missing_binary = str(Path(temporary) / "missing-ironcrew")
            common = [
                sys.executable,
                str(BASE_DIR / "evaluate.py"),
                "--mode",
                "live",
                "--repetitions",
                "5",
                "--binary",
                missing_binary,
                "--dry-run-plan",
            ]
            wrong_model = subprocess.run(
                [*common, "--provider-id", "openai-api", "--model", "gpt-5.6-terra"],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertNotEqual(wrong_model.returncode, 0)
            self.assertIn("requires --model gpt-5.6-luna", wrong_model.stderr)
            self.assertNotIn("binary is not executable", wrong_model.stderr)

            wrong_provider = subprocess.run(
                [*common, "--provider-id", "azure-openai", "--model", "gpt-5.6-luna"],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertNotEqual(wrong_provider.returncode, 0)
            self.assertIn("requires --provider-id openai-api", wrong_provider.stderr)
            self.assertNotIn("binary is not executable", wrong_provider.stderr)

if __name__ == "__main__":
    unittest.main()
