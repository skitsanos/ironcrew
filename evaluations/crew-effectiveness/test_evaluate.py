from __future__ import annotations

import copy
import json
import shutil
import unittest
from collections import Counter
from pathlib import Path
from unittest import mock

import evaluate
import evaluation_setup


BASE_DIR = Path(__file__).resolve().parent


class DatasetTests(unittest.TestCase):
    def load_dataset(self) -> tuple[list[dict], list[dict]]:
        return (
            evaluate.load_jsonl(BASE_DIR / "cases.v1.jsonl"),
            evaluate.load_jsonl(BASE_DIR / "oracle.v1.jsonl"),
        )

    def test_source_options_and_hidden_oracle_are_separate_and_aligned(self) -> None:
        cases, oracle = self.load_dataset()
        indexed = evaluate.validate_dataset(cases, oracle)
        self.assertEqual(len(cases), 6)
        self.assertEqual(set(indexed), {case["case_id"] for case in cases})
        for case in cases:
            self.assertFalse(
                evaluate.nested_keys(case)
                & {
                    "accepted_answers",
                    "correct_option_ids",
                    "citation_sets",
                    "is_correct",
                    "oracle",
                    "gold",
                }
            )
            for question in case["questions"]:
                self.assertGreaterEqual(len(question["options"]), 3)
                self.assertIn(
                    "insufficient_evidence",
                    {option["id"] for option in question["options"]},
                )
                for option in question["options"]:
                    self.assertEqual(set(option), {"id", "label"})
                    self.assertIsNotNone(evaluate.OPTION_ID_PATTERN.fullmatch(option["id"]))

    def test_dataset_rejects_correctness_marker_in_source_option(self) -> None:
        cases, oracle = self.load_dataset()
        altered = copy.deepcopy(cases)
        altered[0]["questions"][0]["options"][0]["is_correct"] = False
        with self.assertRaisesRegex(ValueError, "leaks scoring keys"):
            evaluate.validate_dataset(altered, oracle)

    def test_correct_option_positions_are_balanced(self) -> None:
        cases, oracles = self.load_dataset()
        oracle_by_case = {record["case_id"]: record for record in oracles}
        positions: list[int] = []
        for case in cases:
            correct_by_question = {
                answer["question_id"]: answer["correct_option_ids"]
                for answer in oracle_by_case[case["case_id"]]["answers"]
            }
            for question in case["questions"]:
                correct_ids = correct_by_question[question["id"]]
                self.assertEqual(len(correct_ids), 1)
                positions.append(
                    next(
                        index
                        for index, option in enumerate(question["options"], start=1)
                        if option["id"] == correct_ids[0]
                    )
                )
        self.assertEqual(Counter(positions), Counter({1: 3, 2: 3, 3: 3, 4: 3}))

    def test_dataset_rejects_non_lowercase_option_id(self) -> None:
        cases, oracle = self.load_dataset()
        altered = copy.deepcopy(cases)
        altered[0]["questions"][0]["options"][0]["id"] = "Option_A"
        with self.assertRaisesRegex(ValueError, "invalid lowercase option id"):
            evaluate.validate_dataset(altered, oracle)

    def test_dataset_rejects_oracle_option_not_listed_in_source(self) -> None:
        cases, oracle = self.load_dataset()
        altered = copy.deepcopy(oracle)
        altered[0]["answers"][0]["correct_option_ids"] = ["not_a_listed_option"]
        with self.assertRaisesRegex(ValueError, "unknown correct option IDs"):
            evaluate.validate_dataset(cases, altered)

    def test_dataset_rejects_duplicate_option_labels(self) -> None:
        cases, oracle = self.load_dataset()
        altered = copy.deepcopy(cases)
        options = altered[0]["questions"][0]["options"]
        options[1]["label"] = options[0]["label"].upper()
        with self.assertRaisesRegex(ValueError, "duplicate option label"):
            evaluate.validate_dataset(altered, oracle)

    def test_case_byte_accounting_matches_the_actual_ascii_safe_packet(self) -> None:
        cases = [{"case_id": "unicode", "evidence": "café"}]
        packet = json.dumps(cases[0], sort_keys=True, separators=(",", ":"))
        self.assertEqual(
            evaluation_setup.serialized_case_input_bytes(cases), len(packet.encode("utf-8"))
        )
        self.assertIn(r"\u00e9", packet)


class ScorerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.case = {
            "case_id": "scorer-case",
            "evidence": [{"id": "E1", "text": "Fact"}, {"id": "E2", "text": "More"}],
            "questions": [
                {
                    "id": "Q1",
                    "prompt": "Question",
                    "options": [
                        {"id": "wrong_answer", "label": "Wrong answer"},
                        {"id": "right_answer", "label": "Right answer"},
                        {"id": "insufficient_evidence", "label": "Insufficient evidence"},
                    ],
                }
            ],
        }
        self.oracle = {
            "case_id": "scorer-case",
            "answers": [
                {
                    "question_id": "Q1",
                    "correct_option_ids": ["right_answer"],
                    "citation_sets": [["E1", "E2"]],
                }
            ],
        }

    def output(self, answer: str, citations: list[str] | None = None) -> dict:
        return {
            "case_id": "scorer-case",
            "answers": [
                {
                    "question_id": "Q1",
                    "answer": answer,
                    "citations": ["E1", "E2"] if citations is None else citations,
                }
            ],
        }

    def test_exact_option_id_with_grounding_scores_one(self) -> None:
        output = self.output("right_answer")
        self.assertEqual(evaluate.validate_model_output(output, self.case), [])
        score = evaluate.score_model_output(output, self.case, self.oracle)
        self.assertEqual(score["correctness"], 1.0)
        self.assertEqual(score["grounded_correctness"], 1.0)
        self.assertEqual(score["citation_f1"], 1.0)

    def test_option_label_is_rejected_and_scores_incorrect(self) -> None:
        output = self.output("Right answer")
        errors = evaluate.validate_model_output(output, self.case)
        self.assertTrue(any("exactly match an option id" in error for error in errors))
        self.assertEqual(evaluate.score_model_output(output, self.case, self.oracle)["correctness"], 0.0)

    def test_changed_case_is_rejected_and_scores_incorrect(self) -> None:
        output = self.output("RIGHT_ANSWER")
        errors = evaluate.validate_model_output(output, self.case)
        self.assertTrue(any("exactly match an option id" in error for error in errors))
        self.assertEqual(evaluate.score_model_output(output, self.case, self.oracle)["correctness"], 0.0)

    def test_surrounding_whitespace_is_rejected_and_scores_incorrect(self) -> None:
        output = self.output("right_answer ")
        errors = evaluate.validate_model_output(output, self.case)
        self.assertTrue(any("exactly match an option id" in error for error in errors))
        self.assertEqual(evaluate.score_model_output(output, self.case, self.oracle)["correctness"], 0.0)

    def test_wrong_listed_option_is_contract_valid_but_incorrect(self) -> None:
        output = self.output("wrong_answer")
        self.assertEqual(evaluate.validate_model_output(output, self.case), [])
        score = evaluate.score_model_output(output, self.case, self.oracle)
        self.assertEqual(score["correctness"], 0.0)
        self.assertEqual(score["grounded_correctness"], 0.0)

    def test_correct_option_with_bad_citation_is_not_grounded(self) -> None:
        output = self.output("right_answer", ["E9"])
        score = evaluate.score_model_output(output, self.case, self.oracle)
        self.assertEqual(score["correctness"], 1.0)
        self.assertEqual(score["grounded_correctness"], 0.0)
        self.assertEqual(score["citation_fp"], 1)
        self.assertEqual(score["citation_fn"], 2)
        self.assertEqual(score["answer_details"][0]["unknown_citations"], ["E9"])

    def test_duplicate_question_is_rejected_by_output_contract(self) -> None:
        answer = self.output("right_answer")["answers"][0]
        output = {"case_id": "scorer-case", "answers": [answer, answer]}
        errors = evaluate.validate_model_output(output, self.case)
        self.assertTrue(any("duplicates question_id" in error for error in errors))

    def test_missing_question_is_rejected_by_output_contract(self) -> None:
        output = {"case_id": "scorer-case", "answers": []}
        errors = evaluate.validate_model_output(output, self.case)
        self.assertIn("answers must contain every question exactly once", errors)


class PairwiseTests(unittest.TestCase):
    @staticmethod
    def make_run(variant: str, score: float, tokens: int, duration: int) -> dict:
        return {
            "case_id": "case-a",
            "repetition": 0,
            "variant": variant,
            "grounded_correctness": score,
            "execution_ok": True,
            "output_parse_ok": True,
            "output_schema_ok": True,
            "total_tokens": tokens,
            "run_duration_ms": duration,
        }

    def test_pairwise_metrics_use_matched_runs(self) -> None:
        comparisons = evaluate.pairwise_comparisons(
            [
                self.make_run("single", 0.5, 100, 50),
                self.make_run("dag", 0.75, 250, 100),
                self.make_run("collaborative", 0.25, 300, 150),
            ],
            {
                "familywise_confidence_level": 0.95,
                "multiplicity_correction": "bonferroni",
                "comparison_count": 2,
                "bootstrap_samples": 100,
                "bootstrap_seed": 7,
            },
        )
        dag = comparisons[0]
        self.assertEqual(dag["mean_grounded_correctness_delta"], 0.25)
        self.assertEqual(dag["unique_case_count"], 1)
        self.assertEqual(dag["mean_token_multiplier"], 2.5)
        self.assertEqual(dag["mean_latency_multiplier"], 2.0)
        self.assertEqual(dag["token_pair_count"], 1)
        self.assertEqual(dag["latency_pair_count"], 1)
        self.assertEqual(dag["candidate_success_rate"], 1.0)
        self.assertEqual(
            dag["mean_grounded_correctness_delta_interval"]["lower"], 0.25
        )

    def test_failed_cli_usage_is_unknown_instead_of_zero(self) -> None:
        false_binary = shutil.which("false")
        if false_binary is None:
            self.skipTest("false executable is unavailable")
        result = evaluate.run_one(
            binary=Path(false_binary),
            repo_root=BASE_DIR,
            flow_dir=BASE_DIR,
            case={"case_id": "failed-run"},
            oracle={"answers": []},
            variant="single",
            repetition=0,
            model="offline",
            mode="live",
            timeout_seconds=1,
            planned_llm_calls=1,
            task_llm_calls={"final": 1},
            task_maximum_output_tokens={"final": 800},
            base_environment={},
            mock_server=None,
        )
        self.assertIsNone(result["total_tokens"])
        self.assertIsNone(result["cached_tokens"])

    def test_non_success_run_record_usage_is_unknown(self) -> None:
        for status in ("Failed", "PartialFailure"):
            with self.subTest(status=status), mock.patch.object(
                evaluate.subprocess,
                "run",
                return_value=mock.Mock(
                    returncode=0,
                    stdout=json.dumps(
                        {
                            "run_id": f"{status.casefold()}-run",
                            "status": status,
                            "total_tokens": 123,
                            "cached_tokens": 45,
                            "task_results": [],
                        }
                    ),
                    stderr="",
                ),
            ):
                result = evaluate.run_one(
                    binary=Path("unused-mocked-binary"),
                    repo_root=BASE_DIR,
                    flow_dir=BASE_DIR,
                    case={"case_id": f"{status.casefold()}-case"},
                    oracle={"answers": []},
                    variant="single",
                    repetition=0,
                    model="offline",
                    mode="live",
                    timeout_seconds=1,
                    planned_llm_calls=1,
                    task_llm_calls={"final": 1},
                    task_maximum_output_tokens={"final": 800},
                    base_environment={},
                    mock_server=None,
                )
                self.assertEqual(result["run_status"], status)
                self.assertIsNone(result["total_tokens"])
                self.assertIsNone(result["cached_tokens"])
                self.assertEqual(result["failure_reason"], f"run status was {status}")

    def test_summary_does_not_label_partial_usage_as_total(self) -> None:
        runs = []
        for variant, tokens, duration in (
            ("single", None, None),
            ("dag", 20, 10),
            ("collaborative", 30, 15),
        ):
            runs.append(
                {
                    "variant": variant,
                    "answers_total": 1,
                    "answers_correct": 0,
                    "grounded_correct": 0,
                    "citation_tp": 0,
                    "citation_fp": 0,
                    "citation_fn": 0,
                    "execution_ok": False,
                    "output_parse_ok": False,
                    "output_schema_ok": False,
                    "run_duration_ms": duration,
                    "total_tokens": tokens,
                }
            )
        summaries = {
            item["variant"]: item
            for item in evaluate.summarize_runs(runs, evaluate.VARIANTS)
        }
        self.assertIsNone(summaries["single"]["tokens"]["total"])
        self.assertFalse(summaries["single"]["tokens"]["coverage_complete"])
        self.assertFalse(summaries["single"]["latency_ms"]["coverage_complete"])
        self.assertEqual(summaries["dag"]["tokens"]["total"], 20)


class ExecutionBoundaryTests(unittest.TestCase):
    def test_schema_validation_environment_excludes_provider_credentials(self) -> None:
        minimized = evaluate.schema_validation_environment(
            {
                "PATH": "/bin",
                "SSL_CERT_FILE": "/tmp/cert.pem",
                "TMPDIR": "/tmp",
                "IRONCREW_LOG": "error",
                "OPENAI_API_KEY": "secret-canary",
                "OPENAI_BASE_URL": "https://api.openai.com/v1",
                "OPENAI_MODEL": "gpt-5.6-luna",
                "IRONCREW_PROVIDER_MAX_REQUEST_BYTES": "18000",
            }
        )
        self.assertEqual(
            minimized,
            {
                "PATH": "/bin",
                "SSL_CERT_FILE": "/tmp/cert.pem",
                "TMPDIR": "/tmp",
                "IRONCREW_LOG": "error",
            },
        )

    def test_live_controls_force_reviewed_byte_and_pacing_boundaries(self) -> None:
        environment: dict[str, str] = {}
        plan = {
            "limits": {"max_provider_request_body_bytes": 18_000},
            "rate_limit": {"minimum_provider_start_interval_ms": 3_200},
        }
        evaluate.apply_live_provider_controls(environment, plan)
        self.assertEqual(environment["IRONCREW_PROVIDER_MAX_REQUEST_BYTES"], "18000")
        self.assertEqual(environment["IRONCREW_RATE_LIMIT_MS"], "3200")

    def test_live_process_pacer_waits_after_success_or_failure_completion(self) -> None:
        now = [10.0]
        sleeps: list[float] = []
        pacer = evaluate.LiveProcessPacer(
            3_200,
            clock=lambda: now[0],
            sleeper=lambda delay: sleeps.append(delay),
        )
        pacer.wait_before_start()
        pacer.record_completion()
        now[0] = 11.0
        pacer.wait_before_start()
        self.assertEqual(len(sleeps), 1)
        self.assertAlmostEqual(sleeps[0], 2.2)


if __name__ == "__main__":
    unittest.main()
