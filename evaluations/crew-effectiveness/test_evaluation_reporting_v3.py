from __future__ import annotations

import unittest

from evaluation_reporting_v3 import pricing_receipt, successful_run_usage


def fixture_usage(prompt, completion, total, cached, requests=1):
    fields = dict(prompt_tokens=prompt, completion_tokens=completion,
                  total_tokens=total, cached_tokens=cached)
    return {
        "coverage": "complete", "in_flight": "0",
        "settled": {
            "coverage": "complete", "requests": str(requests),
            **{key: {"known": str(value), "complete": True} for key, value in fields.items()},
            "cache_write_tokens": {"known": None, "complete": False},
            "reasoning_tokens": {"known": None, "complete": False},
        },
    }


class EvaluationReportingV3Tests(unittest.TestCase):
    def test_untrusted_checked_usage_fails_closed(self):
        import copy
        baseline = fixture_usage(10, 2, 12, 0)
        for path, value in [
            (("in_flight",), "1"),
            (("coverage",), "partial"),
            (("settled", "requests"), "2"),
            (("settled", "total_tokens", "known"), 12),
            (("settled", "total_tokens", "known"), "012"),
            (("settled", "total_tokens", "known"), str(2**64)),
            (("settled", "cached_tokens", "complete"), False),
            (("settled", "cached_tokens", "known"), None),
            (("settled", "reasoning_tokens", "known"), "3"),
        ]:
            snapshot = copy.deepcopy(baseline)
            target = snapshot
            for key in path[:-1]:
                target = target[key]
            target[path[-1]] = value
            with self.subTest(path=path, value=value), self.assertRaises(ValueError):
                successful_run_usage([{"task": "final", "usage": snapshot}], {"final": 1}, {"final": 20})

    def test_task_usage_is_complete_and_conservatively_priced(self) -> None:
        usage = successful_run_usage(
            [
                {
                    "task": "first",
                    "usage": fixture_usage(100, 20, 120, 10),
                },
                {
                    "task": "final",
                    "usage": fixture_usage(200, 40, 240, 0),
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
                [{"task": "final", "usage": {}}], {"final": 1}, {"final": 800}
            )
        with self.assertRaisesRegex(ValueError, "planned-call costing allowance"):
            successful_run_usage(
                [
                    {
                        "task": "final",
                        "usage": fixture_usage(20_001, 1, 20_002, 0),
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
                    "usage": fixture_usage(40_000, 1_500, 41_500, 0, 3),
                },
                {
                    "task": "final",
                    "usage": fixture_usage(100, 20, 120, 0),
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
            "usage": fixture_usage(0, 0, 0, 0),
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
