#!/usr/bin/env python3
"""Unit tests for the crew-effectiveness Luna pricing envelope."""

from __future__ import annotations

import json
import sys
import unittest
from pathlib import Path


BASE_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(BASE_DIR))

from pricing_budget import (  # noqa: E402
    BudgetExceededError,
    estimate_cost_upper_bound_usd,
    estimate_cost_usd,
    pricing_metadata,
    require_approved_budget,
)


class PricingTests(unittest.TestCase):
    def test_metadata_pins_reviewed_luna_rates(self) -> None:
        self.assertEqual(
            pricing_metadata(),
            {
                "version": "openai-gpt-5.6-luna-2026-08-12",
                "source": "https://developers.openai.com/api/docs/models/gpt-5.6-luna",
                "retrieved_on": "2026-08-12",
                "currency": "USD",
                "usd_per_million_tokens": {
                    "input": 0.2,
                    "cached_input": 0.02,
                    "output": 1.2,
                },
                "cache_write_multiplier": 1.25,
                "long_context_threshold_tokens": 272_000,
                "long_context_multipliers": {"input": 2.0, "output": 1.5},
                "approval_budget_usd": 3.0,
            },
        )

    def test_mixed_token_classes_are_disjoint(self) -> None:
        self.assertEqual(
            estimate_cost_usd(
                prompt_tokens=100_000,
                cached_tokens=20_000,
                cache_write_tokens=30_000,
                completion_tokens=10_000,
            ),
            0.0299,
        )

    def test_long_context_multiplier_starts_above_threshold(self) -> None:
        at_threshold = estimate_cost_usd(
            prompt_tokens=272_000,
            completion_tokens=10_000,
        )
        above_threshold = estimate_cost_usd(
            prompt_tokens=272_001,
            completion_tokens=10_000,
        )
        self.assertEqual(at_threshold, 0.0664)
        self.assertEqual(above_threshold, 0.1268004)

    def test_upper_bound_prices_all_input_as_cache_write(self) -> None:
        self.assertEqual(
            estimate_cost_upper_bound_usd(
                prompt_tokens=10_000_000,
                completion_tokens=294_000,
                max_input_tokens_per_request=50_000,
            ),
            2.8528,
        )

    def test_upper_bound_honors_per_request_long_context_cap(self) -> None:
        self.assertEqual(
            estimate_cost_upper_bound_usd(
                prompt_tokens=1_000_000,
                completion_tokens=100_000,
                max_input_tokens_per_request=272_001,
            ),
            0.68,
        )

    def test_budget_accepts_equal_value_and_rejects_excess(self) -> None:
        self.assertEqual(require_approved_budget(3), 3.0)
        with self.assertRaisesRegex(BudgetExceededError, "exceeds approved budget"):
            require_approved_budget(3.00000001)

    def test_invalid_token_accounting_is_rejected(self) -> None:
        with self.assertRaisesRegex(ValueError, "cannot exceed"):
            estimate_cost_usd(
                prompt_tokens=9,
                cached_tokens=5,
                cache_write_tokens=5,
                completion_tokens=0,
            )
        with self.assertRaisesRegex(ValueError, "non-negative integer"):
            estimate_cost_usd(prompt_tokens=True, completion_tokens=0)


class DecisionPlanV2Tests(unittest.TestCase):
    def test_plan_covers_twelve_cases_five_times_within_budget(self) -> None:
        plan = json.loads((BASE_DIR / "decision-plan.v2.json").read_text())
        self.assertEqual(plan["schema_version"], "ironcrew.crew-eval-plan.v2")
        self.assertEqual(plan["limits"]["max_case_count"], 12)
        self.assertEqual(plan["limits"]["max_repetitions"], 5)
        self.assertEqual(plan["limits"]["max_cli_runs"], 12 * 5 * 3)
        self.assertEqual(plan["limits"]["max_planned_llm_calls"], 12 * 5 * 8)
        self.assertEqual(
            plan["limits"]["input_token_costing_allowance_per_request"], 20_000
        )
        self.assertEqual(
            plan["limits"]["max_planned_input_tokens"],
            plan["limits"]["max_planned_llm_calls"]
            * plan["limits"]["input_token_costing_allowance_per_request"],
        )
        self.assertEqual(plan["limits"]["max_single_case_input_bytes"], 4_096)
        self.assertEqual(
            plan["limits"]["max_planned_output_tokens"],
            12 * 5 * (800 + 1_800 + 2_300),
        )
        self.assertEqual(plan["pricing"], pricing_metadata())
        estimate = estimate_cost_upper_bound_usd(
            prompt_tokens=plan["limits"]["max_planned_input_tokens"],
            completion_tokens=plan["limits"]["max_planned_output_tokens"],
            max_input_tokens_per_request=plan["limits"][
                "input_token_costing_allowance_per_request"
            ],
        )
        self.assertEqual(estimate, 2.7528)
        self.assertEqual(require_approved_budget(estimate), estimate)


if __name__ == "__main__":
    unittest.main()
