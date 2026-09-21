"""Validate checked runtime snapshots before using receipts for costing."""

from typing import Any

FIELDS = ("prompt_tokens", "completion_tokens", "total_tokens", "cached_tokens",
          "cache_write_tokens", "reasoning_tokens")


def decimal_count(value: Any) -> int:
    if (not isinstance(value, str) or not value or len(value) > 20
            or not value.isascii() or not value.isdecimal()
            or (len(value) > 1 and value.startswith("0"))):
        raise ValueError("usage counts must be canonical unsigned decimal strings")
    count = int(value)
    if count > 2**64 - 1:
        raise ValueError("usage count exceeds unsigned 64-bit range")
    return count


def costing_counts(snapshot: Any, planned_calls: int) -> dict[str, int]:
    if (not isinstance(snapshot, dict)
            or set(snapshot) != {"settled", "in_flight", "coverage", "budget"}
            or snapshot["coverage"] != "complete"
            or decimal_count(snapshot["in_flight"]) != 0):
        raise ValueError("incomplete token usage snapshot")
    validate_budget(snapshot["budget"])
    settled = snapshot["settled"]
    if (not isinstance(settled, dict)
            or set(settled) != {*FIELDS, "requests", "coverage"}
            or settled["coverage"] != "complete"):
        raise ValueError("incomplete token usage aggregate")
    if decimal_count(settled["requests"]) != planned_calls:
        raise ValueError("observed usage requests differ from planned calls")
    values: dict[str, int] = {}
    for key in FIELDS:
        field = settled[key]
        if (not isinstance(field, dict) or set(field) != {"known", "complete"}
                or type(field["complete"]) is not bool):
            raise ValueError("malformed token usage field")
        if field["known"] is None:
            if field["complete"]:
                raise ValueError("complete usage requires a known count")
        else:
            values[key] = decimal_count(field["known"])
        if key in FIELDS[:4] and (not field["complete"] or field["known"] is None):
            raise ValueError("incomplete token usage for costing")
    for detail, parent in (("cached_tokens", "prompt_tokens"),
                           ("cache_write_tokens", "prompt_tokens"),
                           ("reasoning_tokens", "completion_tokens")):
        if detail in values and values[detail] > values[parent]:
            raise ValueError("usage subset exceeds its parent count")
    if values.get("cache_write_tokens", 0) + values["cached_tokens"] > values["prompt_tokens"]:
        raise ValueError("cache categories exceed prompt tokens")
    # Optional unknown detail is not replaced with zero in the runtime receipt.
    # Costing retains its conservative existing cache-write allowance.
    return {key: values[key] for key in FIELDS[:4]}


def validate_budget(budget: Any) -> None:
    if (not isinstance(budget, dict)
            or set(budget) != {"state", "limit", "charged", "retained", "reserved", "in_flight"}
            or budget["state"] not in ("disabled", "active")):
        raise ValueError("incomplete or blocked token budget")
    charged, retained, reserved, active = [
        decimal_count(budget[key]) for key in ("charged", "retained", "reserved", "in_flight")
    ]
    if budget["state"] == "disabled":
        if budget["limit"] is not None or any((charged, retained, reserved, active)):
            raise ValueError("invalid disabled token budget")
        return
    limit = decimal_count(budget["limit"])
    if (not 1 <= limit <= 1_000_000_000 or charged + reserved > limit
            or retained > charged or (active == 0) != (reserved == 0)
            or active > reserved):
        raise ValueError("invalid token budget counters")
