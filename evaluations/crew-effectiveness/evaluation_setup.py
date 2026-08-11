"""Fail-before-execution setup for crew-effectiveness evaluations."""

from __future__ import annotations

import hashlib
import json
import re
from pathlib import Path
from typing import Any

from decision_policy import load_plan, preflight


CONTRACT_PROVIDER_ID = "synthetic-oracle-backed-mock"


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def validate_run_request(
    *, mode: str, repetitions: int | None, provider_id: str | None, model: str
) -> tuple[int, str]:
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._:/-]{0,127}", model):
        raise ValueError("--model must be a non-empty, non-secret provider model identifier")
    if repetitions is None:
        if mode == "live":
            raise ValueError("--repetitions is required in live mode")
        repetitions = 1
    if repetitions < 1:
        raise ValueError("--repetitions must be at least 1")
    if mode == "live":
        if not isinstance(provider_id, str) or not re.fullmatch(
            r"[a-z0-9][a-z0-9._-]{0,63}", provider_id
        ):
            raise ValueError(
                "--provider-id is required in live mode and must be a non-secret lowercase slug"
            )
        return repetitions, provider_id
    return repetitions, CONTRACT_PROVIDER_ID


def serialized_case_input_bytes(cases: list[dict[str, Any]]) -> int:
    return sum(
        len(json.dumps(case, sort_keys=True, separators=(",", ":")).encode("utf-8"))
        for case in cases
    )


def prepare_plan(
    *,
    base_dir: Path,
    plan_path: Path,
    cases: list[dict[str, Any]],
    repetitions: int,
    mode: str,
    variants: tuple[str, ...],
) -> tuple[dict[str, Any], dict[str, int], dict[str, int], dict[str, int]]:
    plan = load_plan(plan_path)
    flow_path = base_dir / plan["flow"]["path"]
    if _sha256_file(flow_path) != plan["flow"]["sha256"]:
        raise ValueError(
            "evaluation plan rejected before provider execution: "
            f"{flow_path.name} SHA-256 does not match the reviewed plan"
        )
    planned_calls = {
        name: plan["flow"]["variants"][name]["planned_llm_calls"] for name in variants
    }
    planned_output_tokens = {
        name: plan["flow"]["variants"][name]["maximum_output_tokens"] for name in variants
    }
    planned_work = preflight(
        plan=plan,
        case_input_bytes=serialized_case_input_bytes(cases),
        case_count=len(cases),
        repetitions=repetitions,
        variants=variants,
        calls_per_run=planned_calls,
        output_tokens_per_run=planned_output_tokens,
        require_decision_grade=mode == "live",
    )
    return plan, planned_work, planned_calls, planned_output_tokens


def plan_receipt(
    *, plan: dict[str, Any], plan_path: Path, planned_work: dict[str, int]
) -> dict[str, Any]:
    return {
        **plan,
        "path": str(plan_path),
        "sha256": _sha256_file(plan_path),
        "planned_work": planned_work,
    }
