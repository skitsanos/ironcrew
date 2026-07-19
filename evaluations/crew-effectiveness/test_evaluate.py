from __future__ import annotations

import copy
import unittest
from collections import Counter
from pathlib import Path

import evaluate


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


if __name__ == "__main__":
    unittest.main()
