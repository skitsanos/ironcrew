"""Stable classifications for the bounded IC-018 supplemental profiles."""

from __future__ import annotations

import copy
from typing import Any

from profile_provider import LARGE_RESULT_BYTES


PROFILE_SPECS = (
    {
        "id": "provider_tool",
        "workload_class": "provider_and_counted_http_tool",
        "bounds": {"runs": 1, "provider_calls": 2, "tool_effects": 1},
        "expected_mock_activity": {
            "provider_tool_provider_calls": 2,
            "provider_tool_tool_effects": 1,
        },
    },
    {
        "id": "large_result",
        "workload_class": "bounded_large_provider_result",
        "bounds": {
            "runs": 1,
            "provider_calls": 1,
            "result_bytes": LARGE_RESULT_BYTES,
        },
        "expected_mock_activity": {"large_result_provider_calls": 1},
    },
    {
        "id": "owner_local_conversation_boundary",
        "workload_class": "conversation_warm_owner_and_cold_peer",
        "bounds": {"conversations": 1, "turns": 2, "max_history": 8},
        "expected_mock_activity": {"conversation_provider_calls": 2},
    },
)


def profile_metadata() -> list[dict[str, Any]]:
    """Return independent copies so result enrichment cannot mutate policy."""
    output = []
    for raw in PROFILE_SPECS:
        spec = copy.deepcopy(raw)
        spec.update(
            {
                "execution_evidence_class": "local_process",
                "provider_evidence_class": "bounded_loopback_mock",
                "provider_free": False,
                "live_provider_evidence": False,
                "platform_evidence": None,
                "planned_paid_provider_calls": 0,
                "actual_paid_provider_calls": 0,
                "estimated_paid_provider_cost_usd": 0.0,
                "secret_boundary": (
                    "live provider credentials and base URLs are removed before launch; "
                    "flows receive only a fixed non-secret loopback fixture"
                ),
            }
        )
        output.append(spec)
    return output
