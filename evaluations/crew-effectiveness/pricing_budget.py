"""Bounded GPT-5.6 Luna pricing estimates for crew-effectiveness runs.

Provider token receipts are the input to these helpers.  The results are
estimates, not invoices: provider billing can apply additional account- or
request-specific rules that are not observable to the evaluator.
"""

from __future__ import annotations

import math
from decimal import Decimal
from typing import Any


PRICING_VERSION = "openai-gpt-5.6-luna-2026-08-12"
PRICING_SOURCE = "https://developers.openai.com/api/docs/models/gpt-5.6-luna"
PRICING_RETRIEVED_ON = "2026-08-12"
INPUT_USD_PER_MILLION = Decimal("0.20")
CACHED_INPUT_USD_PER_MILLION = Decimal("0.02")
OUTPUT_USD_PER_MILLION = Decimal("1.20")
CACHE_WRITE_MULTIPLIER = Decimal("1.25")
LONG_CONTEXT_THRESHOLD_TOKENS = 272_000
LONG_CONTEXT_INPUT_MULTIPLIER = Decimal("2")
LONG_CONTEXT_OUTPUT_MULTIPLIER = Decimal("1.5")
APPROVAL_BUDGET_USD = Decimal("3.00")
_MILLION = Decimal(1_000_000)
_COST_PRECISION = Decimal("0.00000001")


class BudgetExceededError(ValueError):
    """Raised when a token-derived upper bound exceeds the approved envelope."""


def pricing_metadata() -> dict[str, Any]:
    """Return the versioned, JSON-serializable pricing contract."""
    return {
        "version": PRICING_VERSION,
        "source": PRICING_SOURCE,
        "retrieved_on": PRICING_RETRIEVED_ON,
        "currency": "USD",
        "usd_per_million_tokens": {
            "input": float(INPUT_USD_PER_MILLION),
            "cached_input": float(CACHED_INPUT_USD_PER_MILLION),
            "output": float(OUTPUT_USD_PER_MILLION),
        },
        "cache_write_multiplier": float(CACHE_WRITE_MULTIPLIER),
        "long_context_threshold_tokens": LONG_CONTEXT_THRESHOLD_TOKENS,
        "long_context_multipliers": {
            "input": float(LONG_CONTEXT_INPUT_MULTIPLIER),
            "output": float(LONG_CONTEXT_OUTPUT_MULTIPLIER),
        },
        "approval_budget_usd": float(APPROVAL_BUDGET_USD),
    }


def _token_count(value: int, label: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise ValueError(f"{label} must be a non-negative integer")
    return value


def _usd(value: float | Decimal, label: str) -> Decimal:
    if isinstance(value, bool) or not isinstance(value, (int, float, Decimal)):
        raise ValueError(f"{label} must be a finite non-negative number")
    if isinstance(value, float) and not math.isfinite(value):
        raise ValueError(f"{label} must be a finite non-negative number")
    amount = Decimal(str(value))
    if not amount.is_finite() or amount < 0:
        raise ValueError(f"{label} must be a finite non-negative number")
    return amount


def _rounded_usd(value: Decimal) -> float:
    return float(value.quantize(_COST_PRECISION))


def estimate_cost_usd(
    *,
    prompt_tokens: int,
    completion_tokens: int,
    cached_tokens: int = 0,
    cache_write_tokens: int = 0,
) -> float:
    """Estimate cost from a single provider request's token receipt.

    Cached and cache-write tokens are disjoint subsets of ``prompt_tokens``.
    The long-context surcharge applies only when that request has more than
    272,000 prompt tokens, not when aggregate benchmark usage crosses it.
    """
    prompt_tokens = _token_count(prompt_tokens, "prompt_tokens")
    completion_tokens = _token_count(completion_tokens, "completion_tokens")
    cached_tokens = _token_count(cached_tokens, "cached_tokens")
    cache_write_tokens = _token_count(cache_write_tokens, "cache_write_tokens")
    if cached_tokens + cache_write_tokens > prompt_tokens:
        raise ValueError(
            "cached_tokens plus cache_write_tokens cannot exceed prompt_tokens"
        )

    ordinary_tokens = prompt_tokens - cached_tokens - cache_write_tokens
    long_context = prompt_tokens > LONG_CONTEXT_THRESHOLD_TOKENS
    input_multiplier = LONG_CONTEXT_INPUT_MULTIPLIER if long_context else Decimal(1)
    output_multiplier = LONG_CONTEXT_OUTPUT_MULTIPLIER if long_context else Decimal(1)
    input_cost = (
        Decimal(ordinary_tokens) * INPUT_USD_PER_MILLION
        + Decimal(cached_tokens) * CACHED_INPUT_USD_PER_MILLION
        + Decimal(cache_write_tokens)
        * INPUT_USD_PER_MILLION
        * CACHE_WRITE_MULTIPLIER
    ) * input_multiplier
    output_cost = (
        Decimal(completion_tokens) * OUTPUT_USD_PER_MILLION * output_multiplier
    )
    return _rounded_usd((input_cost + output_cost) / _MILLION)


def estimate_cost_upper_bound_usd(
    *,
    prompt_tokens: int,
    completion_tokens: int,
    max_input_tokens_per_request: int,
) -> float:
    """Estimate a conservative aggregate cap for a reviewed execution plan.

    All input is charged at the 1.25x cache-write rate.  Long-context
    multipliers are included whenever the plan permits a request above the
    threshold.  This is intentionally conservative and is not an invoice.
    """
    prompt_tokens = _token_count(prompt_tokens, "prompt_tokens")
    completion_tokens = _token_count(completion_tokens, "completion_tokens")
    max_input_tokens_per_request = _token_count(
        max_input_tokens_per_request, "max_input_tokens_per_request"
    )
    long_context = max_input_tokens_per_request > LONG_CONTEXT_THRESHOLD_TOKENS
    input_multiplier = LONG_CONTEXT_INPUT_MULTIPLIER if long_context else Decimal(1)
    output_multiplier = LONG_CONTEXT_OUTPUT_MULTIPLIER if long_context else Decimal(1)
    cost = (
        Decimal(prompt_tokens)
        * INPUT_USD_PER_MILLION
        * CACHE_WRITE_MULTIPLIER
        * input_multiplier
        + Decimal(completion_tokens) * OUTPUT_USD_PER_MILLION * output_multiplier
    ) / _MILLION
    return _rounded_usd(cost)


def require_approved_budget(
    estimated_cost_upper_bound_usd: float | Decimal,
    approval_budget_usd: float | Decimal = APPROVAL_BUDGET_USD,
) -> float:
    """Return the estimate or reject it when it exceeds the approved budget."""
    estimate = _usd(
        estimated_cost_upper_bound_usd, "estimated_cost_upper_bound_usd"
    )
    approval = _usd(approval_budget_usd, "approval_budget_usd")
    if estimate > approval:
        raise BudgetExceededError(
            f"estimated upper-bound cost ${estimate} exceeds approved budget ${approval}"
        )
    return _rounded_usd(estimate)
