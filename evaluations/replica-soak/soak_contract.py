"""Load and sample a predeclared replica-soak retention contract."""

from __future__ import annotations

import hashlib
import json
import math
import threading
import time
from pathlib import Path
from typing import Any, Callable


SCHEMA_VERSION = "ironcrew.replica-soak-contract.v1"
MAX_CONTRACT_BYTES = 64 * 1024
MAX_OBSERVATIONS = 4_000
GAP_REASONS = {"writer_backpressure", "retention", "global_capacity", "owner_lost"}
LATENCY_METRICS = ("p95", "p99", "max")


def _object(value: Any, name: str, keys: set[str]) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ValueError(f"{name} must be an object")
    unknown, missing = set(value) - keys, keys - set(value)
    if unknown or missing:
        details = []
        if missing:
            details.append(f"missing {', '.join(sorted(missing))}")
        if unknown:
            details.append(f"unknown {', '.join(sorted(unknown))}")
        raise ValueError(f"{name}: {'; '.join(details)}")
    return value


def _number(value: Any, name: str, minimum: float, maximum: float) -> int | float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError(f"{name} must be a number")
    if not math.isfinite(value) or not minimum <= value <= maximum:
        raise ValueError(f"{name} must be between {minimum:g} and {maximum:g}")
    return value


def _integer(value: Any, name: str, minimum: int, maximum: int) -> int:
    number = _number(value, name, minimum, maximum)
    if int(number) != number:
        raise ValueError(f"{name} must be an integer")
    return int(number)


def _latency_ceilings(value: Any) -> dict[str, dict[str, float]]:
    if not isinstance(value, dict) or not value:
        raise ValueError("ceilings.tail_latency_ms must be a non-empty object")
    result = {}
    for operation, raw in value.items():
        if not isinstance(operation, str) or not operation or len(operation) > 64:
            raise ValueError("tail latency operation names must contain 1..=64 characters")
        limits = _object(raw, f"tail latency {operation}", set(LATENCY_METRICS))
        normalized = {
            metric: float(
                _number(limits[metric], f"tail latency {operation}.{metric}", 0, 300_000)
            )
            for metric in LATENCY_METRICS
        }
        if not normalized["p95"] <= normalized["p99"] <= normalized["max"]:
            raise ValueError(f"tail latency {operation} must satisfy p95 <= p99 <= max")
        result[operation] = normalized
    return dict(sorted(result.items()))


def validate_contract(value: Any) -> dict[str, Any]:
    root = _object(
        value,
        "contract",
        {
            "schema_version",
            "observation_interval_seconds",
            "tail_intervals",
            "journal",
            "ceilings",
            "requirements",
        },
    )
    if root["schema_version"] != SCHEMA_VERSION:
        raise ValueError(f"schema_version must be {SCHEMA_VERSION}")
    interval = float(
        _number(root["observation_interval_seconds"], "observation interval", 1, 300)
    )
    tail_intervals = _integer(root["tail_intervals"], "tail_intervals", 1, 100)
    journal = _object(
        root["journal"],
        "journal",
        {
            "max_events_per_run",
            "max_bytes_per_run",
            "max_event_bytes",
            "retention_seconds",
            "max_total_events",
            "max_total_bytes",
            "page_max_events",
            "page_max_bytes",
            "poll_interval_ms",
            "read_timeout_ms",
            "write_timeout_ms",
            "prune_batch",
        },
    )
    journal_bounds = {
        "max_events_per_run": (1, 10_000),
        "max_bytes_per_run": (1_024, 64 * 1024**2),
        "max_event_bytes": (1_024, 16 * 1024**2),
        "retention_seconds": (60, 2_592_000),
        "max_total_events": (1, 10_000_000),
        "max_total_bytes": (1_024, 8 * 1024**3),
        "page_max_events": (1, 64),
        "page_max_bytes": (1_024, 64 * 1024**2),
        "poll_interval_ms": (100, 5_000),
        "read_timeout_ms": (100, 30_000),
        "write_timeout_ms": (100, 5_000),
        "prune_batch": (1, 10_000),
    }
    normalized_journal = {
        name: _integer(journal[name], f"journal.{name}", *bounds)
        for name, bounds in journal_bounds.items()
    }
    if normalized_journal["prune_batch"] > normalized_journal["max_total_events"]:
        raise ValueError("journal.prune_batch must not exceed max_total_events")
    if normalized_journal["max_events_per_run"] > normalized_journal["max_total_events"]:
        raise ValueError("journal.max_events_per_run must not exceed max_total_events")
    if normalized_journal["max_bytes_per_run"] > normalized_journal["max_total_bytes"]:
        raise ValueError("journal.max_bytes_per_run must not exceed max_total_bytes")
    if normalized_journal["max_event_bytes"] > normalized_journal["max_bytes_per_run"]:
        raise ValueError("journal.max_event_bytes must not exceed max_bytes_per_run")
    if normalized_journal["max_event_bytes"] > normalized_journal["page_max_bytes"]:
        raise ValueError("journal.max_event_bytes must not exceed page_max_bytes")
    if normalized_journal["page_max_events"] != min(
        normalized_journal["max_events_per_run"], 64
    ):
        raise ValueError("journal.page_max_events must equal min(max_events_per_run, 64)")
    ceiling_names = {
        "retained_rows",
        "retained_bytes",
        "expired_physical_rows",
        "post_prune_growth_rows",
        "post_prune_growth_bytes",
        "readiness_failures",
        "liveness_failures",
        "rss_peak_bytes_per_replica",
        "tail_rss_growth_bytes_per_replica",
        "tail_run_events_relation_growth_bytes",
        "prefix_relation_bytes",
        "prefix_relation_bytes_per_success",
        "wal_bytes",
        "wal_bytes_per_success",
        "tail_latency_ms",
    }
    ceilings = _object(root["ceilings"], "ceilings", ceiling_names)
    normalized_ceilings = {
        name: _integer(ceilings[name], f"ceilings.{name}", 0, 2**63 - 1)
        for name in ceiling_names - {"tail_latency_ms"}
    }
    normalized_ceilings["tail_latency_ms"] = _latency_ceilings(
        ceilings["tail_latency_ms"]
    )
    requirement_names = {
        "minimum_intervals",
        "minimum_post_boundary_intervals",
        "minimum_prune_intervals",
        "minimum_journal_gap_events",
        "allowed_journal_gap_reasons",
        "require_cursor_expired",
        "minimum_workload_seconds",
        "require_duration_stop",
    }
    requirements = _object(root["requirements"], "requirements", requirement_names)
    reasons = requirements["allowed_journal_gap_reasons"]
    if (
        not isinstance(reasons, list)
        or not reasons
        or len(set(reasons)) != len(reasons)
        or any(reason not in GAP_REASONS for reason in reasons)
    ):
        raise ValueError("allowed journal-gap reasons must be a unique non-empty known list")
    require_expired = requirements["require_cursor_expired"]
    if not isinstance(require_expired, bool):
        raise ValueError("requirements.require_cursor_expired must be boolean")
    require_duration_stop = requirements["require_duration_stop"]
    if not isinstance(require_duration_stop, bool):
        raise ValueError("requirements.require_duration_stop must be boolean")
    normalized_requirements = {
        "minimum_intervals": _integer(
            requirements["minimum_intervals"], "requirements.minimum_intervals", 2, 4_000
        ),
        "minimum_post_boundary_intervals": _integer(
            requirements["minimum_post_boundary_intervals"],
            "requirements.minimum_post_boundary_intervals",
            1,
            4_000,
        ),
        "minimum_prune_intervals": _integer(
            requirements["minimum_prune_intervals"],
            "requirements.minimum_prune_intervals",
            1,
            4_000,
        ),
        "minimum_journal_gap_events": _integer(
            requirements["minimum_journal_gap_events"],
            "requirements.minimum_journal_gap_events",
            0,
            10_000,
        ),
        "allowed_journal_gap_reasons": reasons,
        "require_cursor_expired": require_expired,
        "minimum_workload_seconds": float(
            _number(
                requirements["minimum_workload_seconds"],
                "requirements.minimum_workload_seconds",
                1,
                3_600,
            )
        ),
        "require_duration_stop": require_duration_stop,
    }
    if tail_intervals > normalized_requirements["minimum_intervals"]:
        raise ValueError("tail_intervals must not exceed minimum_intervals")
    return {
        "schema_version": SCHEMA_VERSION,
        "observation_interval_seconds": interval,
        "tail_intervals": tail_intervals,
        "journal": normalized_journal,
        "ceilings": normalized_ceilings,
        "requirements": normalized_requirements,
    }


def load_contract(path: Path) -> tuple[dict[str, Any], str]:
    raw = path.read_bytes()
    if not raw or len(raw) > MAX_CONTRACT_BYTES:
        raise ValueError(f"contract must contain 1..={MAX_CONTRACT_BYTES} bytes")
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ValueError(f"contract is not valid UTF-8 JSON: {error}") from error
    return validate_contract(value), hashlib.sha256(raw).hexdigest()


class IntervalRecorder:
    """Collect bounded PostgreSQL and drained operation snapshots."""

    def __init__(
        self,
        snapshot: Callable[[], dict[str, Any]],
        operations: Callable[[], dict[str, Any]],
        interval_seconds: float,
    ) -> None:
        self.snapshot, self.operations = snapshot, operations
        self.interval_seconds = interval_seconds
        self.started = time.monotonic()
        self.stop_event = threading.Event()
        self.thread: threading.Thread | None = None
        self.observations: list[dict[str, Any]] = []

    def _capture(self, label: str, postgres: dict[str, Any] | None = None) -> None:
        if len(self.observations) >= MAX_OBSERVATIONS:
            return
        try:
            snapshot = postgres if postgres is not None else self.snapshot()
            observation = {"postgres": snapshot}
        except Exception as error:  # background errors must remain visible evidence
            observation = {
                "observation_error": f"{type(error).__name__}: {str(error)[:300]}"
            }
        self.observations.append(
            {
                "label": label,
                "elapsed_seconds": round(time.monotonic() - self.started, 3),
                **observation,
                "operations": self.operations(),
            }
        )

    def start(self, initial_snapshot: dict[str, Any]) -> None:
        self._capture("baseline", initial_snapshot)
        self.thread = threading.Thread(target=self._run, name="soak-contract", daemon=True)
        self.thread.start()

    def _run(self) -> None:
        while not self.stop_event.wait(self.interval_seconds):
            self._capture("interval")

    def stop(self) -> list[dict[str, Any]]:
        self.stop_event.set()
        if self.thread:
            self.thread.join(timeout=max(2.0, self.interval_seconds + 1.0))
            if self.thread.is_alive():
                raise RuntimeError("contract observation thread did not stop")
            self.thread = None
        self._capture("final")
        return self.observations
