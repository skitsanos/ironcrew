from __future__ import annotations

import unittest
from pathlib import Path

import decision_analysis
import decision_policy


BASE_DIR = Path(__file__).resolve().parent


def comparison(
    candidate: str,
    *,
    paired_count: int = 30,
    delta: float = 0.1,
    lower: float = 0.02,
    success_rate: float = 1.0,
    baseline_success_rate: float | None = None,
    token_multiplier: float = 2.0,
    latency_multiplier: float = 2.0,
) -> dict:
    return {
        "candidate": candidate,
        "baseline": "single",
        "paired_count": paired_count,
        "unique_case_count": 6,
        "mean_grounded_correctness_delta": delta,
        "mean_grounded_correctness_delta_interval": {
            "method": "paired-case-bootstrap-percentile-v1",
            "confidence_level": 0.975,
            "familywise_confidence_level": 0.95,
            "multiplicity_correction": "bonferroni",
            "comparison_count": 2,
            "samples": 10_000,
            "seed": 1,
            "lower": lower,
            "upper": delta + 0.05,
        },
        "candidate_success_rate": success_rate,
        "baseline_success_rate": success_rate
        if baseline_success_rate is None
        else baseline_success_rate,
        "token_pair_count": paired_count,
        "latency_pair_count": paired_count,
        "mean_token_multiplier": token_multiplier,
        "mean_latency_multiplier": latency_multiplier,
        "wins": paired_count,
        "ties": 0,
        "losses": 0,
    }


class DecisionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.plan = decision_policy.load_plan(BASE_DIR / "decision-plan.v1.json")

    def test_bootstrap_is_deterministic_and_handles_constant_samples(self) -> None:
        values = [-0.2, 0.1, 0.3, 0.4]
        first = decision_analysis.bootstrap_mean_interval(values, self.plan["uncertainty"], 7)
        second = decision_analysis.bootstrap_mean_interval(values, self.plan["uncertainty"], 7)
        self.assertEqual(first, second)
        constant = decision_analysis.bootstrap_mean_interval(
            [0.25, 0.25], self.plan["uncertainty"], 8
        )
        self.assertEqual((constant["lower"], constant["upper"]), (0.25, 0.25))

    def test_contract_mode_never_claims_effectiveness(self) -> None:
        decision = decision_analysis.topology_decision(
            mode="contract", comparisons=[comparison("dag")], plan=self.plan
        )
        self.assertEqual(decision["status"], "not_applicable")
        self.assertIsNone(decision["recommended_variant"])

    def test_live_decision_distinguishes_sample_gap_single_and_crew(self) -> None:
        insufficient = decision_analysis.topology_decision(
            mode="live",
            comparisons=[comparison("dag", paired_count=29)],
            plan=self.plan,
        )
        self.assertEqual(insufficient["status"], "insufficient_evidence")

        single = decision_analysis.topology_decision(
            mode="live",
            comparisons=[comparison("dag", delta=0.01, lower=-0.1)],
            plan=self.plan,
        )
        self.assertEqual(single["status"], "single_preferred")
        self.assertEqual(single["recommended_variant"], "single")

        unreliable_baseline = decision_analysis.topology_decision(
            mode="live",
            comparisons=[comparison("dag", baseline_success_rate=0.9)],
            plan=self.plan,
        )
        self.assertEqual(unreliable_baseline["status"], "insufficient_evidence")

        crew = decision_analysis.topology_decision(
            mode="live",
            comparisons=[
                comparison("dag", delta=0.12),
                comparison("collaborative", delta=0.08),
            ],
            plan=self.plan,
        )
        self.assertEqual(crew["status"], "crew_qualified")
        self.assertEqual(crew["recommended_variant"], "dag")


if __name__ == "__main__":
    unittest.main()
