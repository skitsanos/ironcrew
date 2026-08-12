"""Matched-pair measurements for crew-effectiveness reports."""

from __future__ import annotations

import math
import statistics
from typing import Any

from decision_analysis import bootstrap_mean_interval


def _percentile_95(values: list[int]) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    return float(ordered[max(0, math.ceil(0.95 * len(ordered)) - 1)])


def summarize_runs(
    runs: list[dict[str, Any]], variants: tuple[str, ...]
) -> list[dict[str, Any]]:
    summaries: list[dict[str, Any]] = []
    for variant in variants:
        selected = [run for run in runs if run["variant"] == variant]
        answers_total = sum(run["answers_total"] for run in selected)
        correct = sum(run["answers_correct"] for run in selected)
        grounded = sum(run["grounded_correct"] for run in selected)
        citation_tp = sum(run["citation_tp"] for run in selected)
        citation_fp = sum(run["citation_fp"] for run in selected)
        citation_fn = sum(run["citation_fn"] for run in selected)
        precision = citation_tp / (citation_tp + citation_fp) if citation_tp + citation_fp else 0.0
        recall = citation_tp / (citation_tp + citation_fn) if citation_tp + citation_fn else 0.0
        f1 = 2 * precision * recall / (precision + recall) if precision + recall else 0.0
        durations = [
            int(run["run_duration_ms"])
            for run in selected
            if isinstance(run.get("run_duration_ms"), int)
        ]
        tokens = [int(run["total_tokens"]) for run in selected if isinstance(run.get("total_tokens"), int)]
        summaries.append(
            {
                "variant": variant,
                "run_count": len(selected),
                "execution_success_rate": sum(run["execution_ok"] for run in selected)
                / len(selected),
                "parse_success_rate": sum(run["output_parse_ok"] for run in selected)
                / len(selected),
                "schema_success_rate": sum(run["output_schema_ok"] for run in selected)
                / len(selected),
                "correctness": correct / answers_total if answers_total else 0.0,
                "grounded_correctness": grounded / answers_total if answers_total else 0.0,
                "citation_precision": precision,
                "citation_recall": recall,
                "citation_f1": f1,
                "latency_ms": {
                    "median": float(statistics.median(durations)) if durations else None,
                    "p95": _percentile_95(durations),
                    "observed_count": len(durations),
                    "coverage_complete": len(durations) == len(selected),
                },
                "tokens": {
                    "median": float(statistics.median(tokens)) if tokens else None,
                    "total": sum(tokens) if len(tokens) == len(selected) else None,
                    "observed_count": len(tokens),
                    "coverage_complete": len(tokens) == len(selected),
                },
            }
        )
    return summaries


def _successful_run(run: dict[str, Any]) -> bool:
    return bool(run["execution_ok"] and run["output_parse_ok"] and run["output_schema_ok"])


def pairwise_comparisons(
    runs: list[dict[str, Any]], uncertainty_policy: dict[str, Any]
) -> list[dict[str, Any]]:
    indexed = {
        (run["case_id"], run["repetition"], run["variant"]): run for run in runs
    }
    comparisons: list[dict[str, Any]] = []
    for variant_index, variant in enumerate(("dag", "collaborative"), start=1):
        deltas: list[float] = []
        deltas_by_case: dict[str, list[float]] = {}
        candidate_successes = baseline_successes = 0
        token_multipliers: list[float] = []
        latency_multipliers: list[float] = []
        wins = ties = losses = 0
        candidate_runs = sorted(
            (
                (case_id, repetition, run)
                for (case_id, repetition, run_variant), run in indexed.items()
                if run_variant == variant
            ),
            key=lambda item: (item[0], item[1]),
        )
        for case_id, repetition, candidate in candidate_runs:
            baseline = indexed.get((case_id, repetition, "single"))
            if baseline is None:
                continue
            delta = candidate["grounded_correctness"] - baseline["grounded_correctness"]
            deltas.append(delta)
            deltas_by_case.setdefault(case_id, []).append(delta)
            candidate_successes += _successful_run(candidate)
            baseline_successes += _successful_run(baseline)
            candidate_tokens = candidate.get("total_tokens")
            baseline_tokens = baseline.get("total_tokens")
            if (
                isinstance(candidate_tokens, int)
                and isinstance(baseline_tokens, int)
                and candidate_tokens > 0
                and baseline_tokens > 0
            ):
                token_multipliers.append(candidate_tokens / baseline_tokens)
            candidate_latency = candidate.get("run_duration_ms")
            baseline_latency = baseline.get("run_duration_ms")
            if (
                isinstance(candidate_latency, int)
                and isinstance(baseline_latency, int)
                and candidate_latency > 0
                and baseline_latency > 0
            ):
                latency_multipliers.append(candidate_latency / baseline_latency)
            if delta > 0:
                wins += 1
            elif delta < 0:
                losses += 1
            else:
                ties += 1
        comparisons.append(
            {
                "candidate": variant,
                "baseline": "single",
                "paired_count": len(deltas),
                "unique_case_count": len(deltas_by_case),
                "mean_grounded_correctness_delta": statistics.fmean(deltas)
                if deltas
                else None,
                "mean_grounded_correctness_delta_interval": bootstrap_mean_interval(
                    [statistics.fmean(values) for values in deltas_by_case.values()],
                    uncertainty_policy,
                    variant_index,
                ),
                "candidate_success_rate": candidate_successes / len(deltas)
                if deltas
                else 0.0,
                "baseline_success_rate": baseline_successes / len(deltas) if deltas else 0.0,
                "token_pair_count": len(token_multipliers),
                "latency_pair_count": len(latency_multipliers),
                "mean_token_multiplier": statistics.fmean(token_multipliers)
                if token_multipliers
                else None,
                "mean_latency_multiplier": statistics.fmean(latency_multipliers)
                if latency_multipliers
                else None,
                "wins": wins,
                "ties": ties,
                "losses": losses,
            }
        )
    return comparisons
