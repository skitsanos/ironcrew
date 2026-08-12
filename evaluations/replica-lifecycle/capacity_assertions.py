"""Pass/fail assertions for the frozen IC-020 capacity envelope."""

from __future__ import annotations

from typing import Any

from capacity_config import (
    ACTIVE_RUNS_PER_REPLICA,
    DB_POOL_PER_REPLICA,
    JOURNAL_MAX_TOTAL_BYTES,
    JOURNAL_MAX_TOTAL_EVENTS,
    MAX_EVENTS_PER_RUN,
    REPLAY_BYTES_PER_RUN,
    SSE_PER_REPLICA,
)


def validate_process_metrics(name: str, metrics: dict[str, int]) -> None:
    expected_exact = {
        "ironcrew_process_active_runs": ACTIVE_RUNS_PER_REPLICA,
        "ironcrew_process_active_runs_limit": ACTIVE_RUNS_PER_REPLICA,
        "ironcrew_process_active_sse_connections": SSE_PER_REPLICA,
        "ironcrew_process_active_sse_connections_limit": SSE_PER_REPLICA,
        "ironcrew_postgres_pool_connections_limit": DB_POOL_PER_REPLICA,
        "ironcrew_process_active_provider_calls": ACTIVE_RUNS_PER_REPLICA,
        "ironcrew_process_peak_active_provider_calls": ACTIVE_RUNS_PER_REPLICA,
        "ironcrew_process_eventbus_instances": ACTIVE_RUNS_PER_REPLICA,
        "ironcrew_process_eventbus_retained_events_capacity": (
            ACTIVE_RUNS_PER_REPLICA * MAX_EVENTS_PER_RUN
        ),
        "ironcrew_process_eventbus_retained_bytes_capacity": (
            ACTIVE_RUNS_PER_REPLICA * REPLAY_BYTES_PER_RUN
        ),
    }
    if any(metrics[key] != value for key, value in expected_exact.items()):
        raise RuntimeError(f"unexpected process metrics on {name}: {metrics}")

    open_connections = metrics["ironcrew_postgres_pool_open_connections"]
    in_use_connections = metrics["ironcrew_postgres_pool_in_use_connections"]
    retained_events = metrics["ironcrew_process_eventbus_retained_events"]
    retained_bytes = metrics["ironcrew_process_eventbus_retained_bytes"]
    if not 1 <= open_connections <= DB_POOL_PER_REPLICA:
        raise RuntimeError(f"unexpected PostgreSQL pool use on {name}: {metrics}")
    if not 0 <= in_use_connections <= open_connections:
        raise RuntimeError(f"invalid PostgreSQL in-use count on {name}: {metrics}")
    if not 1 <= retained_events <= ACTIVE_RUNS_PER_REPLICA * MAX_EVENTS_PER_RUN:
        raise RuntimeError(f"invalid retained event count on {name}: {metrics}")
    if not 1 <= retained_bytes <= ACTIVE_RUNS_PER_REPLICA * REPLAY_BYTES_PER_RUN:
        raise RuntimeError(f"invalid retained event bytes on {name}: {metrics}")

    memory_available = metrics["ironcrew_process_memory_measurement_available"]
    resident = metrics.get("ironcrew_process_resident_memory_bytes")
    resident_peak = metrics.get("ironcrew_process_peak_resident_memory_bytes")
    valid_memory = memory_available in {0, 1}
    valid_memory &= memory_available != 1 or (
        isinstance(resident, int)
        and isinstance(resident_peak, int)
        and 0 < resident <= resident_peak
    )
    valid_memory &= memory_available != 0 or (resident is None and resident_peak is None)
    if not valid_memory:
        raise RuntimeError(f"invalid optional process-memory metrics on {name}: {metrics}")


def validate_journal(snapshot: dict[str, Any]) -> None:
    journal = snapshot["journal"]
    checks = (
        journal["retained_events"] == journal["actual_rows"],
        journal["retained_bytes"] == journal["accounted_bytes"],
        journal["actual_rows"] <= JOURNAL_MAX_TOTAL_EVENTS,
        journal["accounted_bytes"] <= JOURNAL_MAX_TOTAL_BYTES,
        journal["maximum_run_events"] <= MAX_EVENTS_PER_RUN,
        journal["maximum_run_accounted_bytes"] <= REPLAY_BYTES_PER_RUN,
    )
    if not all(checks):
        raise RuntimeError(f"journal accounting exceeded or diverged from bounds: {journal}")
