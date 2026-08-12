"""Uncertainty intervals and predeclared crew-topology decisions."""

from __future__ import annotations

import math
import random
import statistics
from typing import Any


def bootstrap_mean_interval(
    values: list[float], policy: dict[str, Any], seed_offset: int
) -> dict[str, Any]:
    familywise_confidence = policy["familywise_confidence_level"]
    comparison_count = policy["comparison_count"]
    confidence = 1.0 - (1.0 - familywise_confidence) / comparison_count
    samples = policy["bootstrap_samples"]
    seed = policy["bootstrap_seed"] + seed_offset
    if not values:
        lower = upper = None
    elif len(set(values)) == 1:
        lower = upper = float(values[0])
    else:
        rng = random.Random(seed)
        means = sorted(
            statistics.fmean(rng.choice(values) for _ in values) for _ in range(samples)
        )
        tail = (1.0 - confidence) / 2.0
        lower = means[max(0, math.floor(tail * (samples - 1)))]
        upper = means[min(samples - 1, math.ceil((1.0 - tail) * (samples - 1)))]
    return {
        "method": "paired-case-bootstrap-percentile-v1",
        "confidence_level": confidence,
        "familywise_confidence_level": familywise_confidence,
        "multiplicity_correction": policy["multiplicity_correction"],
        "comparison_count": comparison_count,
        "samples": samples,
        "seed": seed,
        "lower": lower,
        "upper": upper,
    }


def topology_decision(
    *, mode: str, comparisons: list[dict[str, Any]], plan: dict[str, Any]
) -> dict[str, Any]:
    if mode != "live":
        return {
            "status": "not_applicable",
            "recommended_variant": None,
            "reason": "contract mode is oracle-backed harness validation, not effectiveness evidence",
            "candidates": [],
        }

    thresholds = plan["selection"]
    assessments: list[dict[str, Any]] = []
    for comparison in comparisons:
        interval = comparison["mean_grounded_correctness_delta_interval"]
        checks = {
            "paired_count": comparison["paired_count"] >= thresholds["minimum_paired_count"],
            "unique_case_count": comparison["unique_case_count"]
            >= thresholds["minimum_unique_case_count"],
            "candidate_success": comparison["candidate_success_rate"]
            >= thresholds["minimum_success_rate"],
            "baseline_success": comparison["baseline_success_rate"]
            >= thresholds["minimum_success_rate"],
            "token_coverage": comparison["token_pair_count"] == comparison["paired_count"],
            "latency_coverage": comparison["latency_pair_count"]
            == comparison["paired_count"],
            "mean_quality_delta": comparison["mean_grounded_correctness_delta"] is not None
            and comparison["mean_grounded_correctness_delta"]
            >= thresholds["minimum_mean_grounded_correctness_delta"],
            "confidence_lower_bound": interval["lower"] is not None
            and interval["lower"] >= thresholds["minimum_confidence_lower_bound"],
            "token_multiplier": comparison["mean_token_multiplier"] is not None
            and comparison["mean_token_multiplier"]
            <= thresholds["maximum_mean_token_multiplier"],
            "latency_multiplier": comparison["mean_latency_multiplier"] is not None
            and comparison["mean_latency_multiplier"]
            <= thresholds["maximum_mean_latency_multiplier"],
        }
        assessments.append(
            {
                "candidate": comparison["candidate"],
                "qualified": all(checks.values()),
                "checks": checks,
            }
        )

    incomplete_checks = (
        "paired_count",
        "unique_case_count",
        "baseline_success",
        "token_coverage",
        "latency_coverage",
    )
    if not assessments or any(
        not all(item["checks"][check] for check in incomplete_checks) for item in assessments
    ):
        status = "insufficient_evidence"
        recommendation = None
        reason = "the predeclared paired sample or resource-measurement coverage was not met"
    else:
        qualified = [item for item in assessments if item["qualified"]]
        if not qualified:
            status = "single_preferred"
            recommendation = "single"
            reason = (
                "no crew topology met every predeclared quality, reliability, cost, "
                "and latency threshold"
            )
        else:
            by_candidate = {item["candidate"]: item for item in comparisons}
            qualified.sort(
                key=lambda item: (
                    -by_candidate[item["candidate"]]["mean_grounded_correctness_delta"],
                    by_candidate[item["candidate"]]["mean_token_multiplier"],
                    by_candidate[item["candidate"]]["mean_latency_multiplier"],
                    item["candidate"],
                )
            )
            status = "crew_qualified"
            recommendation = qualified[0]["candidate"]
            reason = "the recommended crew topology met every predeclared threshold"
    return {
        "status": status,
        "recommended_variant": recommendation,
        "reason": reason,
        "candidates": assessments,
    }
