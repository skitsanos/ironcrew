from __future__ import annotations

import unittest

from evaluation_reporting_v3 import pricing_receipt, successful_run_usage


class EvaluationReportingV3Tests(unittest.TestCase):
    def test_task_usage_is_complete_and_conservatively_priced(self) -> None:
        usage = successful_run_usage(
            [
                {
                    "task": "first",
                    "token_usage": {
                        "prompt_tokens": 100,
                        "completion_tokens": 20,
                        "total_tokens": 120,
                        "cached_tokens": 10,
                    },
                },
                {
                    "task": "final",
                    "token_usage": {
                        "prompt_tokens": 200,
                        "completion_tokens": 40,
                        "total_tokens": 240,
                        "cached_tokens": 0,
                    },
                },
            ],
            {"first": 1, "final": 1},
            {"first": 800, "final": 800},
            input_token_costing_allowance_per_request=20_000,
            max_completion_tokens_per_request=800,
        )
        self.assertEqual(usage["prompt_tokens"], 300)
        self.assertEqual(usage["completion_tokens"], 60)
        self.assertEqual(usage["total_tokens"], 360)
        self.assertEqual(usage["cached_tokens"], 10)
        self.assertGreater(usage["estimated_cost_upper_bound_usd"], 0)
        self.assertEqual(usage["task_usage"][0]["planned_llm_calls"], 1)
        self.assertEqual(
            usage["task_usage"][0]["prompt_token_costing_allowance"], 20_000
        )

    def test_incomplete_or_over_cap_usage_fails_closed(self) -> None:
        with self.assertRaisesRegex(ValueError, "incomplete token usage"):
            successful_run_usage(
                [{"task": "final", "token_usage": {}}], {"final": 1}, {"final": 800}
            )
        with self.assertRaisesRegex(ValueError, "planned-call costing allowance"):
            successful_run_usage(
                [
                    {
                        "task": "final",
                        "token_usage": {
                            "prompt_tokens": 20_001,
                            "completion_tokens": 1,
                            "total_tokens": 20_002,
                            "cached_tokens": 0,
                        },
                    }
                ],
                {"final": 1},
                {"final": 800},
                input_token_costing_allowance_per_request=20_000,
                max_completion_tokens_per_request=800,
            )

    def test_collaborative_task_uses_three_call_aggregate_allowances(self) -> None:
        usage = successful_run_usage(
            [
                {
                    "task": "discussion",
                    "token_usage": {
                        "prompt_tokens": 40_000,
                        "completion_tokens": 1_500,
                        "total_tokens": 41_500,
                        "cached_tokens": 0,
                    },
                },
                {
                    "task": "final",
                    "token_usage": {
                        "prompt_tokens": 100,
                        "completion_tokens": 20,
                        "total_tokens": 120,
                        "cached_tokens": 0,
                    },
                },
            ],
            {"discussion": 3, "final": 1},
            {"discussion": 1_500, "final": 800},
            input_token_costing_allowance_per_request=20_000,
            max_completion_tokens_per_request=800,
        )
        discussion = usage["task_usage"][0]
        self.assertEqual(discussion["planned_llm_calls"], 3)
        self.assertEqual(discussion["prompt_token_costing_allowance"], 60_000)
        self.assertEqual(discussion["completion_token_limit"], 1_500)

    def test_zero_or_missing_planned_task_usage_fails_closed(self) -> None:
        zero = {
            "task": "final",
            "token_usage": {
                "prompt_tokens": 0,
                "completion_tokens": 0,
                "total_tokens": 0,
                "cached_tokens": 0,
            },
        }
        with self.assertRaisesRegex(ValueError, "zero prompt, completion, or total"):
            successful_run_usage([zero], {"final": 1}, {"final": 800})
        with self.assertRaisesRegex(ValueError, "planned task call mapping"):
            successful_run_usage([], {"final": 1}, {"final": 800})

    def test_live_pricing_requires_complete_observed_usage(self) -> None:
        complete = pricing_receipt(
            mode="live",
            runs=[{"estimated_cost_upper_bound_usd": 0.1}],
            planned_upper_bound_usd=2.7528,
        )
        self.assertTrue(complete["coverage_complete"])
        self.assertTrue(complete["planned_bound_within_budget"])
        self.assertTrue(complete["observed_estimate_within_budget"])
        missing = pricing_receipt(
            mode="live",
            runs=[{"estimated_cost_upper_bound_usd": None}],
            planned_upper_bound_usd=2.7528,
        )
        self.assertFalse(missing["coverage_complete"])
        self.assertIsNone(missing["observed_estimated_upper_bound_usd"])


if __name__ == "__main__":
    unittest.main()
