"""Fail-before-execution setup for crew-effectiveness evaluations."""

from __future__ import annotations

import json
import re
from typing import Any


CONTRACT_PROVIDER_ID = "synthetic-oracle-backed-mock"
REQUIRED_LIVE_MODEL = "gpt-5.6-luna"
REQUIRED_LIVE_PROVIDER_ID = "openai-api"


def validate_run_request(
    *, mode: str, repetitions: int | None, provider_id: str | None, model: str
) -> tuple[int, str]:
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._:/-]{0,127}", model):
        raise ValueError("--model must be a non-empty, non-secret provider model identifier")
    if model != REQUIRED_LIVE_MODEL:
        raise ValueError(f"IC-009 requires --model {REQUIRED_LIVE_MODEL}")
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
        if provider_id != REQUIRED_LIVE_PROVIDER_ID:
            raise ValueError(
                f"live IC-009 requires --provider-id {REQUIRED_LIVE_PROVIDER_ID}"
            )
        return repetitions, provider_id
    return repetitions, CONTRACT_PROVIDER_ID


def serialized_case_input_bytes(cases: list[dict[str, Any]]) -> int:
    return sum(
        len(json.dumps(case, sort_keys=True, separators=(",", ":")).encode("utf-8"))
        for case in cases
    )
