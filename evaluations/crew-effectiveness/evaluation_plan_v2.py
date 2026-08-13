"""Strict preflight for the decision-grade crew-effectiveness plan."""

from __future__ import annotations

import hashlib
import json
import math
import re
from pathlib import Path
from typing import Any

from pricing_budget import (
    estimate_cost_upper_bound_usd,
    pricing_metadata,
    require_approved_budget,
)


SCHEMA_VERSION = "ironcrew.crew-eval-plan.v2"
VARIANTS = {
    "single": {
        "planned_llm_calls": 1,
        "maximum_output_tokens": 800,
        "task_llm_calls": {"final": 1},
        "task_maximum_output_tokens": {"final": 800},
    },
    "dag": {
        "planned_llm_calls": 3,
        "maximum_output_tokens": 1_800,
        "task_llm_calls": {"extract": 1, "challenge": 1, "final": 1},
        "task_maximum_output_tokens": {"extract": 500, "challenge": 500, "final": 800},
    },
    "collaborative": {
        "planned_llm_calls": 4,
        "maximum_output_tokens": 2_300,
        "task_llm_calls": {"discussion": 3, "final": 1},
        "task_maximum_output_tokens": {"discussion": 1_500, "final": 800},
    },
}
LIMIT_KEYS = {
    "max_case_count",
    "max_repetitions",
    "max_cli_runs",
    "max_planned_llm_calls",
    "max_planned_input_tokens",
    "input_token_costing_allowance_per_request",
    "max_provider_request_body_bytes",
    "max_completion_tokens_per_request",
    "max_planned_output_tokens",
    "max_case_input_bytes",
    "max_single_case_input_bytes",
}
EXPECTED_LIMITS = {
    "max_case_count": 12,
    "max_repetitions": 5,
    "max_cli_runs": 180,
    "max_planned_llm_calls": 480,
    "max_planned_input_tokens": 9_600_000,
    "input_token_costing_allowance_per_request": 20_000,
    "max_provider_request_body_bytes": 18_000,
    "max_completion_tokens_per_request": 800,
    "max_planned_output_tokens": 294_000,
    "max_case_input_bytes": 65_536,
    "max_single_case_input_bytes": 4_096,
}
RATE_LIMIT_KEYS = {
    "minimum_provider_start_interval_ms",
    "rolling_window_seconds",
    "maximum_provider_starts_per_window",
    "maximum_token_allowance_per_window",
    "reference_rpm",
    "reference_tpm",
    "reference",
}
EXPECTED_RATE_LIMIT = {
    "minimum_provider_start_interval_ms": 3_200,
    "rolling_window_seconds": 60,
    "maximum_provider_starts_per_window": 19,
    "maximum_token_allowance_per_window": 395_200,
    "reference_rpm": 500,
    "reference_tpm": 500_000,
    "reference": "openai-gpt-5.6-luna-tier-1-2026-08-12",
}
DATASET_PACK_KEYS = {
    "pack_id",
    "pack_version",
    "case_count",
    "manifest_sha256",
    "cases_sha256",
    "oracle_sha256",
}


def _object(value: Any, label: str, keys: set[str]) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != keys:
        raise ValueError(f"{label} must contain exactly {sorted(keys)}")
    return value


def _integer(value: Any, label: str, minimum: int, maximum: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or not minimum <= value <= maximum:
        raise ValueError(f"{label} must be an integer between {minimum} and {maximum}")
    return value


def _number(value: Any, label: str, minimum: float, maximum: float) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError(f"{label} must be a number between {minimum} and {maximum}")
    result = float(value)
    if not math.isfinite(result) or not minimum <= result <= maximum:
        raise ValueError(f"{label} must be a number between {minimum} and {maximum}")
    return result


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def _dataset_identity(receipt: dict[str, Any]) -> dict[str, Any]:
    """Reduce a validated loader receipt to the exact decision-plan identity."""
    return {
        "aggregate_sha256": receipt["aggregate_sha256"],
        "packs": [
            {key: pack[key] for key in DATASET_PACK_KEYS}
            for pack in receipt["packs"]
        ],
    }


def _validate_dataset_identity(value: Any) -> dict[str, Any]:
    dataset = _object(value, "dataset", {"aggregate_sha256", "packs"})
    if not isinstance(dataset["aggregate_sha256"], str) or not re.fullmatch(
        r"[a-f0-9]{64}", dataset["aggregate_sha256"]
    ):
        raise ValueError("dataset aggregate SHA-256 is invalid")
    if not isinstance(dataset["packs"], list) or not dataset["packs"]:
        raise ValueError("dataset packs must be a non-empty ordered array")
    packs: list[dict[str, Any]] = []
    seen: set[str] = set()
    for index, raw in enumerate(dataset["packs"]):
        pack = _object(raw, f"dataset pack {index}", DATASET_PACK_KEYS)
        pack_id = pack["pack_id"]
        if not isinstance(pack_id, str) or not re.fullmatch(r"[a-z0-9][a-z0-9-]{0,63}", pack_id):
            raise ValueError(f"dataset pack {index} has invalid pack_id")
        if pack_id in seen:
            raise ValueError(f"dataset contains duplicate pack_id {pack_id}")
        seen.add(pack_id)
        normalized = {
            "pack_id": pack_id,
            "pack_version": _integer(pack["pack_version"], "pack version", 1, 1_000_000),
            "case_count": _integer(pack["case_count"], "pack case count", 1, 10_000),
        }
        for key in ("cases_sha256", "oracle_sha256"):
            if not isinstance(pack[key], str) or not re.fullmatch(r"[a-f0-9]{64}", pack[key]):
                raise ValueError(f"dataset pack {index} has invalid {key}")
            normalized[key] = pack[key]
        manifest = pack["manifest_sha256"]
        if manifest is not None and (
            not isinstance(manifest, str) or not re.fullmatch(r"[a-f0-9]{64}", manifest)
        ):
            raise ValueError(f"dataset pack {index} has invalid manifest_sha256")
        normalized["manifest_sha256"] = manifest
        packs.append(normalized)
    return {"aggregate_sha256": dataset["aggregate_sha256"], "packs": packs}


def load_plan(path: Path, base_dir: Path) -> dict[str, Any]:
    """Load and validate every decision-bearing plan field."""
    try:
        plan = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"invalid evaluation plan {path}: {error}") from error
    plan = _object(
        plan,
        "evaluation plan",
        {
            "schema_version",
            "dataset",
            "flow",
            "limits",
            "rate_limit",
            "pricing",
            "uncertainty",
            "selection",
        },
    )
    if plan["schema_version"] != SCHEMA_VERSION:
        raise ValueError(f"evaluation plan schema_version must be {SCHEMA_VERSION}")
    dataset = _validate_dataset_identity(plan["dataset"])
    flow = _object(plan["flow"], "flow", {"path", "sha256", "variants"})
    if flow["path"] != "crew.lua" or not re.fullmatch(r"[a-f0-9]{64}", flow["sha256"]):
        raise ValueError("flow path or SHA-256 is invalid")
    if flow["variants"] != VARIANTS:
        raise ValueError("flow variant accounting must match the reviewed crew.lua budget")
    for variant, accounting in VARIANTS.items():
        if sum(accounting["task_llm_calls"].values()) != accounting["planned_llm_calls"]:
            raise ValueError(f"flow {variant} task call accounting is inconsistent")
        if (
            sum(accounting["task_maximum_output_tokens"].values())
            != accounting["maximum_output_tokens"]
        ):
            raise ValueError(f"flow {variant} task output accounting is inconsistent")
    flow_path = base_dir / "crew.lua"
    if _sha256(flow_path) != flow["sha256"]:
        raise ValueError("evaluation plan rejected: crew.lua SHA-256 does not match")

    limits = _object(plan["limits"], "limits", LIMIT_KEYS)
    normalized_limits = {
        key: _integer(value, f"limits.{key}", 1, 100_000_000)
        for key, value in limits.items()
    }
    if normalized_limits["max_planned_input_tokens"] != (
        normalized_limits["max_planned_llm_calls"]
        * normalized_limits["input_token_costing_allowance_per_request"]
    ):
        raise ValueError(
            "planned input-token costing allowance must equal calls times per-request "
            "costing allowance"
        )
    if normalized_limits != EXPECTED_LIMITS:
        raise ValueError("limits must match the reviewed IC-009 execution envelope")
    rate_limit = _object(plan["rate_limit"], "rate_limit", RATE_LIMIT_KEYS)
    normalized_rate_limit = {
        key: (
            rate_limit[key]
            if key == "reference"
            else _integer(rate_limit[key], f"rate_limit.{key}", 1, 100_000_000)
        )
        for key in RATE_LIMIT_KEYS
    }
    if normalized_rate_limit != EXPECTED_RATE_LIMIT:
        raise ValueError("rate_limit must match the reviewed IC-009 pacing envelope")
    starts_from_interval = (
        (
            normalized_rate_limit["rolling_window_seconds"] * 1_000 - 1
        )
        // normalized_rate_limit["minimum_provider_start_interval_ms"]
        + 1
    )
    if starts_from_interval != normalized_rate_limit["maximum_provider_starts_per_window"]:
        raise ValueError("provider-start interval does not derive the rolling-window start bound")
    token_allowance = normalized_rate_limit["maximum_provider_starts_per_window"] * (
        normalized_limits["input_token_costing_allowance_per_request"]
        + normalized_limits["max_completion_tokens_per_request"]
    )
    if token_allowance != normalized_rate_limit["maximum_token_allowance_per_window"]:
        raise ValueError("rolling-window token allowance does not match starts times allowances")
    if normalized_rate_limit["maximum_provider_starts_per_window"] > (
        normalized_rate_limit["reference_rpm"]
    ) or normalized_rate_limit["maximum_token_allowance_per_window"] > (
        normalized_rate_limit["reference_tpm"]
    ):
        raise ValueError("reviewed pacing envelope exceeds its reference rate limits")
    if plan["pricing"] != pricing_metadata():
        raise ValueError("pricing declaration does not match the reviewed Luna contract")
    upper_bound = estimate_cost_upper_bound_usd(
        prompt_tokens=normalized_limits["max_planned_input_tokens"],
        completion_tokens=normalized_limits["max_planned_output_tokens"],
        # This allowance is for conservative cost planning. The independent
        # runtime boundary is the serialized provider-request byte limit.
        max_input_tokens_per_request=normalized_limits[
            "input_token_costing_allowance_per_request"
        ],
    )
    require_approved_budget(upper_bound, plan["pricing"]["approval_budget_usd"])

    uncertainty = _object(
        plan["uncertainty"],
        "uncertainty",
        {
            "familywise_confidence_level",
            "multiplicity_correction",
            "comparison_count",
            "bootstrap_samples",
            "bootstrap_seed",
        },
    )
    if uncertainty["multiplicity_correction"] != "bonferroni":
        raise ValueError("multiplicity correction must be bonferroni")
    normalized_uncertainty = {
        "familywise_confidence_level": _number(
            uncertainty["familywise_confidence_level"], "familywise confidence", 0.5, 0.999
        ),
        "multiplicity_correction": "bonferroni",
        "comparison_count": _integer(uncertainty["comparison_count"], "comparison count", 2, 2),
        "bootstrap_samples": _integer(
            uncertainty["bootstrap_samples"], "bootstrap samples", 100, 100_000
        ),
        "bootstrap_seed": _integer(uncertainty["bootstrap_seed"], "bootstrap seed", 0, 2**63 - 1),
    }
    selection = _object(
        plan["selection"],
        "selection",
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
        "minimum_paired_count": _integer(selection["minimum_paired_count"], "minimum pairs", 1, 10_000),
        "minimum_unique_case_count": _integer(
            selection["minimum_unique_case_count"], "minimum unique cases", 1, 10_000
        ),
        "minimum_success_rate": _number(selection["minimum_success_rate"], "success rate", 0, 1),
        "minimum_mean_grounded_correctness_delta": _number(
            selection["minimum_mean_grounded_correctness_delta"], "quality delta", -1, 1
        ),
        "minimum_confidence_lower_bound": _number(
            selection["minimum_confidence_lower_bound"], "confidence bound", -1, 1
        ),
        "maximum_mean_token_multiplier": _number(
            selection["maximum_mean_token_multiplier"], "token multiplier", 0.01, 100
        ),
        "maximum_mean_latency_multiplier": _number(
            selection["maximum_mean_latency_multiplier"], "latency multiplier", 0.01, 100
        ),
    }
    return {
        "schema_version": SCHEMA_VERSION,
        "dataset": dataset,
        "flow": flow,
        "limits": normalized_limits,
        "rate_limit": normalized_rate_limit,
        "pricing": plan["pricing"],
        "uncertainty": normalized_uncertainty,
        "selection": normalized_selection,
        "planned_cost_upper_bound_usd": upper_bound,
    }


def preflight(
    *, plan: dict[str, Any], corpus_receipt: dict[str, Any], case_sizes: list[int], repetitions: int,
    require_complete: bool = True,
) -> dict[str, Any]:
    """Require the complete predeclared workload before any provider call."""
    if _dataset_identity(corpus_receipt) != plan["dataset"]:
        raise ValueError(
            "evaluation plan rejected before provider execution: "
            "current corpus does not match the frozen dataset identity"
        )
    limits = plan["limits"]
    case_count = len(case_sizes)
    cli_runs = case_count * repetitions * len(VARIANTS)
    calls = case_count * repetitions * sum(item["planned_llm_calls"] for item in VARIANTS.values())
    output_tokens = case_count * repetitions * sum(
        item["maximum_output_tokens"] for item in VARIANTS.values()
    )
    planned_input_allowance = (
        calls * limits["input_token_costing_allowance_per_request"]
    )
    actual = {
        "case_input_bytes": sum(case_sizes),
        "maximum_single_case_input_bytes": max(case_sizes, default=0),
        "cli_runs": cli_runs,
        "llm_calls": calls,
        "input_token_costing_allowance": planned_input_allowance,
        "maximum_output_tokens": output_tokens,
        "paired_comparisons_per_candidate": case_count * repetitions,
        "unique_cases": case_count,
        "repetitions": repetitions,
    }
    expected = {
        "unique_cases": limits["max_case_count"],
        "repetitions": limits["max_repetitions"],
        "cli_runs": limits["max_cli_runs"],
        "llm_calls": limits["max_planned_llm_calls"],
        "input_token_costing_allowance": limits["max_planned_input_tokens"],
        "maximum_output_tokens": limits["max_planned_output_tokens"],
    }
    if require_complete:
        failures = [
            f"{key}={actual[key]} must equal reviewed {value}"
            for key, value in expected.items()
            if actual[key] != value
        ]
    else:
        failures = [
            f"{key}={actual[key]} exceeds reviewed {value}"
            for key, value in expected.items()
            if actual[key] > value
        ]
    if actual["case_input_bytes"] > limits["max_case_input_bytes"]:
        failures.append("aggregate case input exceeds the reviewed byte cap")
    if actual["maximum_single_case_input_bytes"] > limits["max_single_case_input_bytes"]:
        failures.append("one case exceeds the reviewed byte cap")
    if require_complete and actual["paired_comparisons_per_candidate"] < plan["selection"]["minimum_paired_count"]:
        failures.append("paired comparison count is below the decision threshold")
    if failures:
        raise ValueError("evaluation plan rejected before provider execution: " + "; ".join(failures))
    planned_cost = estimate_cost_upper_bound_usd(
        prompt_tokens=planned_input_allowance,
        completion_tokens=output_tokens,
        max_input_tokens_per_request=limits[
            "input_token_costing_allowance_per_request"
        ],
    )
    require_approved_budget(planned_cost, plan["pricing"]["approval_budget_usd"])
    actual["planned_cost_upper_bound_usd"] = planned_cost
    return actual
