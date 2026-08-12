"""Receipts and grouped measurements for crew-effectiveness report v3."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from pairwise_analysis import summarize_runs
from pricing_budget import estimate_cost_usd, pricing_metadata


def _integer(value: Any, label: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise ValueError(f"{label} must be a non-negative integer")
    return value


def successful_run_usage(
    task_results: list[Any],
    task_llm_calls: dict[str, int],
    task_maximum_output_tokens: dict[str, int],
    input_token_costing_allowance_per_request: int | None = None,
    max_completion_tokens_per_request: int | None = None,
) -> dict[str, Any]:
    """Aggregate complete task usage and conservatively price cache writes.

    The input value is a costing allowance checked against provider-reported
    usage after a call. It is not presented as an exact tokenizer or pre-send
    input cap; the live pre-send boundary is the serialized request byte limit.
    """
    prompt = completion = cached = total = 0
    task_receipts: list[dict[str, Any]] = []
    if set(task_llm_calls) != set(task_maximum_output_tokens):
        raise ValueError("planned task call and output mappings differ")
    observed_tasks: set[str] = set()
    for index, task in enumerate(task_results):
        if not isinstance(task, dict):
            raise ValueError(f"task_results[{index}] must be an object")
        task_name = task.get("task")
        if not isinstance(task_name, str) or task_name not in task_llm_calls:
            raise ValueError(f"task_results[{index}] has an unexpected task name")
        if task_name in observed_tasks:
            raise ValueError(f"task_results contains duplicate task {task_name}")
        observed_tasks.add(task_name)
        planned_calls = task_llm_calls[task_name]
        completion_limit = task_maximum_output_tokens[task_name]
        if isinstance(planned_calls, bool) or not isinstance(planned_calls, int) or planned_calls < 1:
            raise ValueError(f"task {task_name} has an invalid planned call count")
        usage = task.get("token_usage")
        if not isinstance(usage, dict) or set(usage) != {
            "prompt_tokens",
            "completion_tokens",
            "total_tokens",
            "cached_tokens",
        }:
            raise ValueError(f"task_results[{index}] has incomplete token usage")
        values = {key: _integer(value, f"task_results[{index}].{key}") for key, value in usage.items()}
        if (
            values["prompt_tokens"] == 0
            or values["completion_tokens"] == 0
            or values["total_tokens"] == 0
        ):
            raise ValueError(
                f"task_results[{index}] has zero prompt, completion, or total token usage"
            )
        if values["cached_tokens"] > values["prompt_tokens"]:
            raise ValueError(f"task_results[{index}] cached tokens exceed prompt tokens")
        if values["total_tokens"] != values["prompt_tokens"] + values["completion_tokens"]:
            raise ValueError(f"task_results[{index}] total token accounting is inconsistent")
        if (
            input_token_costing_allowance_per_request is not None
            and values["prompt_tokens"]
            > input_token_costing_allowance_per_request * planned_calls
        ):
            raise ValueError(
                f"task_results[{index}] prompt tokens exceed its planned-call costing allowance"
            )
        if (
            max_completion_tokens_per_request is not None
            and completion_limit > max_completion_tokens_per_request * planned_calls
        ):
            raise ValueError(
                f"task {task_name} completion limit exceeds its planned-call runtime limit"
            )
        if values["completion_tokens"] > completion_limit:
            raise ValueError(
                f"task_results[{index}] completion tokens exceed its exact task limit"
            )
        noncached = values["prompt_tokens"] - values["cached_tokens"]
        cost = estimate_cost_usd(
            prompt_tokens=values["prompt_tokens"],
            completion_tokens=values["completion_tokens"],
            cached_tokens=values["cached_tokens"],
            cache_write_tokens=noncached,
        )
        task_receipts.append(
            {
                "task": task_name,
                "planned_llm_calls": planned_calls,
                "prompt_token_costing_allowance": (
                    input_token_costing_allowance_per_request * planned_calls
                    if input_token_costing_allowance_per_request is not None
                    else 0
                ),
                "completion_token_limit": completion_limit,
                **values,
                "estimated_cost_upper_bound_usd": cost,
            }
        )
        prompt += values["prompt_tokens"]
        completion += values["completion_tokens"]
        cached += values["cached_tokens"]
        total += values["total_tokens"]
    if observed_tasks != set(task_llm_calls):
        raise ValueError("task_results do not match the planned task call mapping")
    return {
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "cached_tokens": cached,
        "total_tokens": total,
        "estimated_cost_upper_bound_usd": round(
            sum(item["estimated_cost_upper_bound_usd"] for item in task_receipts), 8
        ),
        "task_usage": task_receipts,
    }


def empty_usage() -> dict[str, Any]:
    return {
        "prompt_tokens": None,
        "completion_tokens": None,
        "cached_tokens": None,
        "total_tokens": None,
        "estimated_cost_upper_bound_usd": None,
        "task_usage": [],
    }


def _relative(root: Path, value: str | None) -> str | None:
    if value is None:
        return None
    path = Path(value).resolve(strict=True)
    try:
        return path.relative_to(root.resolve(strict=True)).as_posix()
    except ValueError as error:
        raise ValueError("corpus receipt path is outside the repository") from error


def dataset_receipt(root: Path, receipt: dict[str, Any]) -> dict[str, Any]:
    """Remove absolute paths while retaining every corpus identity."""
    packs: list[dict[str, Any]] = []
    for original in receipt["packs"]:
        pack = dict(original)
        for key in ("manifest_path", "cases_path", "oracle_path"):
            pack[key] = _relative(root, pack[key])
        packs.append(pack)
    return {
        "name": "grounded-decisions-multipack-v1",
        "answer_contract": "source-visible-single-select-v1",
        "correctness_rule": "exact-option-id-v1",
        "case_count": receipt["case_count"],
        "aggregate_sha256": receipt["aggregate_sha256"],
        "oracle_injected_into_prompt": False,
        "packs": packs,
    }


def domain_summaries(
    runs: list[dict[str, Any]], pack_ids: list[str], variants: tuple[str, ...]
) -> list[dict[str, Any]]:
    return [
        {
            "pack_id": pack_id,
            "variants": summarize_runs(
                [run for run in runs if run["domain_pack"] == pack_id], variants
            ),
        }
        for pack_id in sorted(pack_ids)
    ]


def pricing_receipt(
    *, mode: str, runs: list[dict[str, Any]], planned_upper_bound_usd: float,
) -> dict[str, Any]:
    observed = [run.get("estimated_cost_upper_bound_usd") for run in runs]
    coverage = all(isinstance(item, (int, float)) for item in observed)
    observed_total = round(sum(observed), 8) if coverage else None
    approved = pricing_metadata()["approval_budget_usd"]
    planned_within_budget = planned_upper_bound_usd <= approved
    observed_within_budget = (
        observed_total <= approved if mode == "live" and observed_total is not None else None
    )
    return {
        "applicable": mode == "live",
        "contract": pricing_metadata(),
        "planned_upper_bound_usd": planned_upper_bound_usd if mode == "live" else 0.0,
        "observed_estimated_upper_bound_usd": observed_total if mode == "live" else None,
        "coverage_complete": coverage if mode == "live" else False,
        "planned_bound_within_budget": planned_within_budget,
        "observed_estimate_within_budget": observed_within_budget,
        "estimate_not_invoice": True,
        "uncached_input_priced_as_cache_write": True,
    }


def report_json_bytes(report: dict[str, Any]) -> bytes:
    return (json.dumps(report, indent=2, sort_keys=True) + "\n").encode("utf-8")
