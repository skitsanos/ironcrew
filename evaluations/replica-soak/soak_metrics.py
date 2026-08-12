"""Bounded cumulative and interval operation metrics for replica soak."""

from __future__ import annotations

import math
import random
import threading
from collections import Counter
from typing import Any


MAX_LATENCY_SAMPLES = 10_000


def percentile(sorted_values: list[float], quantile: float) -> float | None:
    if not sorted_values:
        return None
    index = max(0, math.ceil(quantile * len(sorted_values)) - 1)
    return round(sorted_values[index], 3)


class OperationMetrics:
    """Thread-safe bounded latency/error accumulator with interval drains."""

    def __init__(self, max_samples: int = MAX_LATENCY_SAMPLES) -> None:
        self._lock = threading.Lock()
        self._max_samples = max_samples
        self._rng = random.Random(0x1A0C0E)
        self._operations: dict[str, dict[str, Any]] = {}
        self._interval_operations: dict[str, dict[str, Any]] = {}

    @staticmethod
    def _new_entry() -> dict[str, Any]:
        return {
            "count": 0,
            "ok": 0,
            "errors": 0,
            "sum_latency_ms": 0.0,
            "max_latency_ms": 0.0,
            "response_bytes": 0,
            "status_counts": Counter(),
            "error_kinds": Counter(),
            "samples": [],
        }

    def _record_entry(
        self,
        entry: dict[str, Any],
        latency_ms: float,
        status: int | None,
        ok: bool,
        error_kind: str | None,
        response_bytes: int,
    ) -> None:
        entry["count"] += 1
        entry["ok" if ok else "errors"] += 1
        entry["sum_latency_ms"] += latency_ms
        entry["max_latency_ms"] = max(entry["max_latency_ms"], latency_ms)
        entry["response_bytes"] += response_bytes
        entry["status_counts"][str(status) if status is not None else "none"] += 1
        if error_kind:
            entry["error_kinds"][error_kind] += 1
        samples: list[float] = entry["samples"]
        if len(samples) < self._max_samples:
            samples.append(latency_ms)
        else:
            replacement = self._rng.randrange(entry["count"])
            if replacement < self._max_samples:
                samples[replacement] = latency_ms

    def record(
        self,
        operation: str,
        latency_ms: float,
        status: int | None,
        ok: bool,
        error_kind: str | None = None,
        response_bytes: int = 0,
    ) -> None:
        with self._lock:
            for entries in (self._operations, self._interval_operations):
                self._record_entry(
                    entries.setdefault(operation, self._new_entry()),
                    latency_ms,
                    status,
                    ok,
                    error_kind,
                    response_bytes,
                )

    def count(self, operation: str) -> int:
        with self._lock:
            return int(self._operations.get(operation, {}).get("count", 0))

    def total_latency_ms(self, operation: str) -> float:
        with self._lock:
            return float(self._operations.get(operation, {}).get("sum_latency_ms", 0.0))

    @staticmethod
    def _report(snapshot: list[tuple[str, dict[str, Any]]]) -> dict[str, Any]:
        output: dict[str, Any] = {}
        for operation, entry in sorted(snapshot):
            samples = sorted(entry["samples"])
            count = entry["count"]
            output[operation] = {
                "count": count,
                "ok": entry["ok"],
                "errors": entry["errors"],
                "error_rate": round(entry["errors"] / count, 6) if count else 0.0,
                "response_bytes": entry["response_bytes"],
                "latency_ms": {
                    "mean": round(entry["sum_latency_ms"] / count, 3) if count else None,
                    "p50": percentile(samples, 0.50),
                    "p95": percentile(samples, 0.95),
                    "p99": percentile(samples, 0.99),
                    "max": round(entry["max_latency_ms"], 3) if count else None,
                    "sampled_count": len(samples),
                    "percentiles_approximate": count > len(samples),
                },
                "status_counts": dict(sorted(entry["status_counts"].items())),
                "error_kinds": dict(sorted(entry["error_kinds"].items())),
            }
        return output

    def report(self) -> dict[str, Any]:
        with self._lock:
            snapshot = list(self._operations.items())
        return self._report(snapshot)

    def interval_report(self) -> dict[str, Any]:
        """Drain metrics accumulated since the prior interval."""
        with self._lock:
            snapshot = list(self._interval_operations.items())
            self._interval_operations = {}
        return self._report(snapshot)
