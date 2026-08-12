"""Signal extraction and focused criteria for replica-soak contracts."""

from __future__ import annotations

from typing import Any


def integer(value: Any) -> int | None:
    return value if isinstance(value, int) and not isinstance(value, bool) else None


def criterion(passed: bool, **evidence: Any) -> dict[str, Any]:
    return {"passed": passed, **evidence}


def maximum(values: list[int | None]) -> int | None:
    available = [value for value in values if value is not None]
    return max(available) if len(available) == len(values) and available else None


def growth(values: list[int | None], start: int = 0) -> int | None:
    selected = values[start:]
    if not selected or any(value is None for value in selected):
        return None
    baseline = selected[0]
    assert baseline is not None
    return max(0, max(value for value in selected if value is not None) - baseline)


def _table(postgres: dict[str, Any], suffix: str) -> dict[str, Any]:
    return next(
        (
            row
            for row in postgres.get("tables", [])
            if str(row.get("relname", "")).endswith(suffix)
        ),
        {},
    )


def signals(observation: dict[str, Any]) -> dict[str, int | None]:
    postgres = observation.get("postgres") or {}
    accounting = postgres.get("journal_accounting") or {}
    retention = postgres.get("retention_state") or {}
    events = _table(postgres, "run_events")
    relation_sizes = [integer(row.get("total_bytes")) for row in postgres.get("tables", [])]
    prefix_bytes = sum(relation_sizes) if relation_sizes and None not in relation_sizes else None
    actual_rows = integer(accounting.get("actual_rows"))
    retained_rows = integer(accounting.get("retained_events"))
    actual_bytes = integer(accounting.get("accounted_bytes"))
    retained_bytes = integer(accounting.get("retained_bytes"))
    return {
        "actual_rows": actual_rows,
        "retained_rows": retained_rows,
        "used_rows": (
            max(actual_rows, retained_rows)
            if actual_rows is not None and retained_rows is not None
            else None
        ),
        "used_bytes": (
            max(actual_bytes, retained_bytes)
            if actual_bytes is not None and retained_bytes is not None
            else None
        ),
        "expired_physical_rows": integer(accounting.get("expired_physical_rows")),
        "aggregate_tuples_deleted": integer(events.get("tuples_deleted")),
        "retention_gap_runs": integer(retention.get("gap_runs")),
        "retention_dropped_sequences": integer(retention.get("dropped_sequences")),
        "run_events_relation_bytes": integer(events.get("total_bytes")),
        "prefix_relation_bytes": prefix_bytes,
        "wal_bytes": integer(postgres.get("wal_bytes_from_origin")),
    }


def health(
    observations: list[dict[str, Any]], operation: str, ceiling: int
) -> dict[str, Any]:
    errors = sum(
        int(item.get("operations", {}).get(operation, {}).get("errors", 0))
        for item in observations
    )
    return criterion(errors <= ceiling, observed=errors, ceiling=ceiling)


def latency(
    tail: list[dict[str, Any]], configured: dict[str, dict[str, float]], required: int
) -> dict[str, Any]:
    violations: list[dict[str, Any]] = []
    evaluated = 0
    for item in tail:
        operations = item.get("operations", {})
        for operation, ceilings in configured.items():
            metrics = operations.get(operation, {})
            latency_values = metrics.get("latency_ms", {})
            count = integer(metrics.get("count")) or 0
            if count == 0:
                violations.append(
                    {
                        "elapsed_seconds": item.get("elapsed_seconds"),
                        "operation": operation,
                        "reason": "no_samples",
                    }
                )
                continue
            evaluated += count
            for metric, ceiling in ceilings.items():
                observed = latency_values.get(metric)
                if not isinstance(observed, (int, float)) or observed > ceiling:
                    violations.append(
                        {
                            "elapsed_seconds": item.get("elapsed_seconds"),
                            "operation": operation,
                            "metric": metric,
                            "observed": observed,
                            "ceiling": ceiling,
                        }
                    )
    return criterion(
        len(tail) == required and evaluated > 0 and not violations,
        evaluated_intervals=len(tail),
        required_intervals=required,
        samples=evaluated,
        ceilings=configured,
        violations=violations[:100],
        violations_truncated=len(violations) > 100,
    )


def rss(
    resources: dict[str, Any], tail_start: float, ceilings: dict[str, Any]
) -> tuple[dict[str, Any], dict[str, Any]]:
    replicas = resources.get("replicas", {})
    peaks = {name: integer(item.get("sampled_peak_rss_bytes")) for name, item in replicas.items()}
    peak_available = len(peaks) == 2 and all(value is not None for value in peaks.values())
    tail_growth: dict[str, int | None] = {}
    for name, item in replicas.items():
        samples = [
            integer(sample.get("rss_bytes"))
            for sample in item.get("timeline", [])
            if isinstance(sample.get("elapsed_s"), (int, float))
            and sample["elapsed_s"] >= tail_start
        ]
        tail_growth[name] = growth(samples)
    growth_available = len(tail_growth) == 2 and all(
        value is not None for value in tail_growth.values()
    )
    peak_ceiling = ceilings["rss_peak_bytes_per_replica"]
    growth_ceiling = ceilings["tail_rss_growth_bytes_per_replica"]
    return (
        criterion(
            peak_available and all(value <= peak_ceiling for value in peaks.values()),
            observed_by_replica=peaks,
            ceiling_per_replica=peak_ceiling,
        ),
        criterion(
            growth_available
            and all(value <= growth_ceiling for value in tail_growth.values()),
            observed_by_replica=tail_growth,
            ceiling_per_replica=growth_ceiling,
            semantics="maximum sampled tail RSS minus first sampled tail RSS",
        ),
    )


def replay(
    probe: dict[str, Any], requirements: dict[str, Any]
) -> tuple[dict[str, Any], dict[str, Any], dict[str, Any]]:
    gap = probe.get("gap_probe", {})
    gap_count, reasons = integer(gap.get("count")), gap.get("reasons")
    terminal = gap.get("terminal", {})
    gap_ok = (
        gap_count is not None
        and gap_count >= requirements["minimum_journal_gap_events"]
        and isinstance(reasons, list)
        and "retention" in reasons
        and all(reason in requirements["allowed_journal_gap_reasons"] for reason in reasons)
        and terminal.get("id") is None
        and terminal.get("status") == "success"
        and terminal.get("journal_complete") is False
        and terminal.get("synthesized_from_run_record") is True
    )
    gap_criterion = criterion(
        gap_ok,
        observed=gap_count,
        minimum=requirements["minimum_journal_gap_events"],
        reasons=reasons,
        terminal=terminal,
        allowed_reasons=requirements["allowed_journal_gap_reasons"],
    )
    cursor = probe.get("cursor_probe", {})
    cursor_criterion = criterion(
        not requirements["require_cursor_expired"]
        or (cursor.get("status") == 409 and cursor.get("code") == "cursor_expired"),
        required=requirements["require_cursor_expired"],
        status=cursor.get("status"),
        code=cursor.get("code"),
    )
    anchor = probe.get("anchor", {})
    anchor_criterion = criterion(
        anchor.get("physical_rows") == 0
        and anchor.get("retained_events") == 0
        and anchor.get("eviction_reason") == "retention"
        and isinstance(anchor.get("dropped_through"), int)
        and anchor["dropped_through"] > 0,
        **anchor,
    )
    return gap_criterion, cursor_criterion, anchor_criterion


def lifecycle(
    evidence: dict[str, Any], base_passed: bool
) -> tuple[dict[str, Any], dict[str, Any], dict[str, Any], dict[str, Any]]:
    shutdown = evidence.get("replica_shutdown", {})
    shutdown_ok = len(shutdown) == 2 and all(
        item.get("exit_code") == 0 and item.get("forced_kill") is False
        for item in shutdown.values()
    )
    cleanup = evidence.get("cleanup", {})
    cleanup_ok = (
        cleanup.get("database_cleanup_requested") is True
        and cleanup.get("database_cleanup_performed") is True
        and cleanup.get("database_cleanup_error") is None
    )
    inventory = evidence.get("post_cleanup_inventory", {})
    inventory_ok = inventory.get("relations") == 0 and inventory.get("functions") == 0
    source_start = evidence.get("source_at_start")
    source_finish = evidence.get("source_at_finish")
    source_ok = bool(source_start) and source_start == source_finish
    return (
        criterion(shutdown_ok and base_passed, replicas=shutdown, pre_shutdown_passed=base_passed),
        criterion(cleanup_ok, **cleanup),
        criterion(inventory_ok, **inventory),
        criterion(
            source_ok,
            start_manifest_sha256=(source_start or {}).get("worktree_manifest_sha256"),
            finish_manifest_sha256=(source_finish or {}).get("worktree_manifest_sha256"),
        ),
    )
