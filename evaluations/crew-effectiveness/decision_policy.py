"""Bounded execution planning and topology decisions for crew evaluation."""

from __future__ import annotations

import json
import math
import re
from pathlib import Path
from typing import Any


PLAN_SCHEMA_VERSION = "ironcrew.crew-eval-plan.v1"
EXPECTED_FLOW_VARIANTS = {
    "single": {"planned_llm_calls": 1, "maximum_output_tokens": 800},
    "dag": {"planned_llm_calls": 3, "maximum_output_tokens": 1_800},
    "collaborative": {"planned_llm_calls": 4, "maximum_output_tokens": 2_300},
}


def _require_object(value: Any, label: str, keys: set[str]) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != keys:
        raise ValueError(f"{label} must contain exactly {sorted(keys)}")
    return value


def _bounded_int(value: Any, label: str, minimum: int, maximum: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or not minimum <= value <= maximum:
        raise ValueError(f"{label} must be an integer between {minimum} and {maximum}")
    return value


def _bounded_number(value: Any, label: str, minimum: float, maximum: float) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError(f"{label} must be a number between {minimum} and {maximum}")
    number = float(value)
    if not math.isfinite(number) or not minimum <= number <= maximum:
        raise ValueError(f"{label} must be a number between {minimum} and {maximum}")
    return number


def validate_plan(value: Any) -> dict[str, Any]:
    plan = _require_object(
        value,
        "evaluation plan",
        {"schema_version", "flow", "limits", "uncertainty", "selection"},
    )
    if plan["schema_version"] != PLAN_SCHEMA_VERSION:
        raise ValueError(f"evaluation plan schema_version must be {PLAN_SCHEMA_VERSION}")

    flow = _require_object(
        plan["flow"],
        "evaluation plan flow",
        {"path", "sha256", "variants"},
    )
    if flow["path"] != "crew.lua":
        raise ValueError("flow.path must be crew.lua")
    if not isinstance(flow["sha256"], str) or not re.fullmatch(
        r"[a-f0-9]{64}", flow["sha256"]
    ):
        raise ValueError("flow.sha256 must be a lowercase SHA-256 digest")
    variants = _require_object(
        flow["variants"],
        "evaluation plan flow variants",
        {"single", "dag", "collaborative"},
    )
    normalized_variants: dict[str, dict[str, int]] = {}
    for name in ("single", "dag", "collaborative"):
        variant = _require_object(
            variants[name],
            f"evaluation plan flow variant {name}",
            {"planned_llm_calls", "maximum_output_tokens"},
        )
        normalized_variants[name] = {
            "planned_llm_calls": _bounded_int(
                variant["planned_llm_calls"],
                f"flow.variants.{name}.planned_llm_calls",
                1,
                100,
            ),
            "maximum_output_tokens": _bounded_int(
                variant["maximum_output_tokens"],
                f"flow.variants.{name}.maximum_output_tokens",
                1,
                1_000_000,
            ),
        }
    if normalized_variants != EXPECTED_FLOW_VARIANTS:
        raise ValueError("flow variant accounting must match the reviewed crew.lua budget")

    limits = _require_object(
        plan["limits"],
        "evaluation plan limits",
        {
            "max_cli_runs",
            "max_planned_llm_calls",
            "max_planned_output_tokens",
            "max_case_input_bytes",
        },
    )
    normalized_limits = {
        "max_cli_runs": _bounded_int(limits["max_cli_runs"], "limits.max_cli_runs", 1, 10_000),
        "max_planned_llm_calls": _bounded_int(
            limits["max_planned_llm_calls"], "limits.max_planned_llm_calls", 1, 100_000
        ),
        "max_planned_output_tokens": _bounded_int(
            limits["max_planned_output_tokens"],
            "limits.max_planned_output_tokens",
            1,
            100_000_000,
        ),
        "max_case_input_bytes": _bounded_int(
            limits["max_case_input_bytes"], "limits.max_case_input_bytes", 1, 10_000_000
        ),
    }

    uncertainty = _require_object(
        plan["uncertainty"],
        "evaluation plan uncertainty",
        {
            "familywise_confidence_level",
            "multiplicity_correction",
            "comparison_count",
            "bootstrap_samples",
            "bootstrap_seed",
        },
    )
    if uncertainty["multiplicity_correction"] != "bonferroni":
        raise ValueError("uncertainty.multiplicity_correction must be bonferroni")
    comparison_count = _bounded_int(
        uncertainty["comparison_count"], "uncertainty.comparison_count", 1, 100
    )
    if comparison_count != len(normalized_variants) - 1:
        raise ValueError("uncertainty.comparison_count must match the two crew comparisons")
    normalized_uncertainty = {
        "familywise_confidence_level": _bounded_number(
            uncertainty["familywise_confidence_level"],
            "uncertainty.familywise_confidence_level",
            0.5,
            0.999,
        ),
        "multiplicity_correction": "bonferroni",
        "comparison_count": comparison_count,
        "bootstrap_samples": _bounded_int(
            uncertainty["bootstrap_samples"], "uncertainty.bootstrap_samples", 100, 100_000
        ),
        "bootstrap_seed": _bounded_int(
            uncertainty["bootstrap_seed"], "uncertainty.bootstrap_seed", 0, 2**63 - 1
        ),
    }

    selection = _require_object(
        plan["selection"],
        "evaluation plan selection",
        {
            "minimum_paired_count",
            "minimum_unique_case_count",
            "minimum_success_rate",
            "minimum_mean_grounded_correctness_delta",
            "minimum_confidence_lower_bound",
            "maximum_mean_token_multiplier",
            "maximum_mean_latency_multiplier",
        },
    )
    normalized_selection = {
        "minimum_paired_count": _bounded_int(
            selection["minimum_paired_count"], "selection.minimum_paired_count", 1, 10_000
        ),
        "minimum_unique_case_count": _bounded_int(
            selection["minimum_unique_case_count"],
            "selection.minimum_unique_case_count",
            1,
            10_000,
        ),
        "minimum_success_rate": _bounded_number(
            selection["minimum_success_rate"], "selection.minimum_success_rate", 0.0, 1.0
        ),
        "minimum_mean_grounded_correctness_delta": _bounded_number(
            selection["minimum_mean_grounded_correctness_delta"],
            "selection.minimum_mean_grounded_correctness_delta",
            -1.0,
            1.0,
        ),
        "minimum_confidence_lower_bound": _bounded_number(
            selection["minimum_confidence_lower_bound"],
            "selection.minimum_confidence_lower_bound",
            -1.0,
            1.0,
        ),
        "maximum_mean_token_multiplier": _bounded_number(
            selection["maximum_mean_token_multiplier"],
            "selection.maximum_mean_token_multiplier",
            0.01,
            100.0,
        ),
        "maximum_mean_latency_multiplier": _bounded_number(
            selection["maximum_mean_latency_multiplier"],
            "selection.maximum_mean_latency_multiplier",
            0.01,
            100.0,
        ),
    }
    return {
        "schema_version": PLAN_SCHEMA_VERSION,
        "flow": {
            "path": "crew.lua",
            "sha256": flow["sha256"],
            "variants": normalized_variants,
        },
        "limits": normalized_limits,
        "uncertainty": normalized_uncertainty,
        "selection": normalized_selection,
    }


def load_plan(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except OSError as error:
        raise ValueError(f"could not read evaluation plan {path}: {error}") from error
    except json.JSONDecodeError as error:
        raise ValueError(f"evaluation plan {path} is invalid JSON: {error}") from error
    return validate_plan(value)


def preflight(
    *,
    plan: dict[str, Any],
    case_input_bytes: int,
    case_count: int,
    repetitions: int,
    variants: tuple[str, ...],
    calls_per_run: dict[str, int],
    output_tokens_per_run: dict[str, int],
    require_decision_grade: bool = False,
) -> dict[str, int]:
    cli_runs = case_count * repetitions * len(variants)
    planned_calls = case_count * repetitions * sum(calls_per_run[name] for name in variants)
    planned_output_tokens = case_count * repetitions * sum(
        output_tokens_per_run[name] for name in variants
    )
    planned = {
        "case_input_bytes": case_input_bytes,
        "cli_runs": cli_runs,
        "llm_calls": planned_calls,
        "maximum_output_tokens": planned_output_tokens,
        "paired_comparisons_per_candidate": case_count * repetitions,
        "unique_cases": case_count,
    }
    checks = (
        ("case_input_bytes", "max_case_input_bytes"),
        ("cli_runs", "max_cli_runs"),
        ("llm_calls", "max_planned_llm_calls"),
        ("maximum_output_tokens", "max_planned_output_tokens"),
    )
    exceeded = [
        f"{actual}={planned[actual]} exceeds {limit}={plan['limits'][limit]}"
        for actual, limit in checks
        if planned[actual] > plan["limits"][limit]
    ]
    if require_decision_grade:
        selection = plan["selection"]
        if planned["paired_comparisons_per_candidate"] < selection["minimum_paired_count"]:
            exceeded.append(
                "paired_comparisons_per_candidate="
                f"{planned['paired_comparisons_per_candidate']} is below "
                f"minimum_paired_count={selection['minimum_paired_count']}"
            )
        if planned["unique_cases"] < selection["minimum_unique_case_count"]:
            exceeded.append(
                f"unique_cases={planned['unique_cases']} is below "
                f"minimum_unique_case_count={selection['minimum_unique_case_count']}"
            )
    if exceeded:
        raise ValueError("evaluation plan rejected before provider execution: " + "; ".join(exceeded))
    return planned
