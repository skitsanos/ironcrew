"""Fixed, reviewable compatibility envelope; no arbitrary paid model selection."""
from datetime import date
from decimal import Decimal, InvalidOperation

MODEL = "gpt-5.6-luna"
RUN_TOKENS = 10_000
OUTPUT_TOKENS = 1_024
TIMEOUT_SECONDS = 300
CAPTURE_BYTES = 1024 * 1024
PRICING_DATE = date(2026, 9, 21)
PRICE_EXPIRY_DAYS = 30
CASES = (
    {"id": "cli-basic", "fixture": "basic.lua", "transport": "cli", "max_requests": 1},
    {"id": "cli-dialog", "fixture": "dialog.lua", "transport": "cli", "max_requests": 2},
    {"id": "cli-hitl", "fixture": "hitl.lua", "transport": "cli-pty", "max_requests": 22},
    {"id": "http-hitl", "fixture": "hitl.lua", "transport": "http", "max_requests": 22},
)


def plan(budget: str = "0.15", *, today: date | None = None) -> dict:
    try:
        approval = Decimal(budget)
    except InvalidOperation:
        raise ValueError("invalid spending estimate ceiling") from None
    # $3/M exceeds current native short-context Fast output ($2.40/M) and
    # cache-write ($0.50/M) prices, even pricing every token as output.
    upper = Decimal(RUN_TOKENS * len(CASES)) * Decimal(3) / Decimal(1_000_000)
    if not approval.is_finite() or not upper <= approval <= Decimal("0.15"):
        raise ValueError("spending estimate ceiling must cover $0.12 and cannot exceed $0.15")
    age = ((today or date.today()) - PRICING_DATE).days
    return {
        "schema": "ironcrew.live-smoke.v1", "model": MODEL,
        "model_identity_scope": "requested current alias, not an immutable provider snapshot",
        "provider": "openai-responses", "origin": "https://api.openai.com",
        "efforts": ["none", "low"], "cases": list(CASES),
        "limits": {"concurrency": 1, "runs": len(CASES),
                   "generation_requests": sum(case["max_requests"] for case in CASES),
                   "input_count_requests": sum(case["max_requests"] for case in CASES),
                   "tokens_per_run": RUN_TOKENS, "total_tokens": RUN_TOKENS * len(CASES),
                   "output_tokens_per_request": OUTPUT_TOKENS,
                   "wall_seconds": TIMEOUT_SECONDS, "capture_bytes_per_stream": CAPTURE_BYTES,
                   "max_tool_rounds_per_task": 10, "task_retries": 0},
        "pricing": {"verified_on": PRICING_DATE.isoformat(), "max_age_days": PRICE_EXPIRY_DAYS,
                    "fresh": 0 <= age <= PRICE_EXPIRY_DAYS,
                    "source": "https://developers.openai.com/api/docs/pricing",
                    "conservative_usd_per_million_tokens": "3.00",
                    "estimated_upper_bound_usd": str(upper), "ceiling_usd": str(approval),
                    "notice": "Estimate, not an invoice; depends on provider token bounds. No hosted tools."},
        "nightly_activation": "disabled until owner approval and protected environment setup",
        "evidence_boundary": "local compatibility, not effectiveness, replicas or deployment",
    }
