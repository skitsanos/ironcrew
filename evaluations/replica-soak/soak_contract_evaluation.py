"""Evaluate replica-soak retention observations against a declaration."""

from __future__ import annotations

from typing import Any

from soak_contract_signals import (
    criterion,
    growth,
    health,
    latency,
    lifecycle,
    maximum,
    replay,
    rss,
    signals,
)


def evaluate_contract(
    contract: dict[str, Any],
    observations: list[dict[str, Any]],
    resources: dict[str, Any],
    replay_probe: dict[str, Any],
    journal_configuration: dict[str, int],
    workload: dict[str, Any],
    lifecycle_evidence: dict[str, Any],
    base_passed: bool,
) -> dict[str, Any]:
    ceilings, requirements = contract["ceilings"], contract["requirements"]
    valid = [item for item in observations if isinstance(item.get("postgres"), dict)]
    extracted = [signals(item) for item in valid]
    prune_indexes = [
        index
        for index in range(1, len(extracted))
        if extracted[index - 1]["retention_dropped_sequences"] is not None
        and extracted[index]["retention_dropped_sequences"] is not None
        and extracted[index]["retention_dropped_sequences"]
        > extracted[index - 1]["retention_dropped_sequences"]
    ]
    boundary = prune_indexes[0] if prune_indexes else None
    post_boundary = len(extracted) - boundary - 1 if boundary is not None else 0
    final_elapsed = float(valid[-1].get("elapsed_seconds", 0)) if valid else 0.0
    tail_start = max(
        0.0,
        final_elapsed
        - contract["tail_intervals"] * contract["observation_interval_seconds"],
    )
    complete_tail = [
        item
        for item in valid
        if item.get("label") == "interval"
        and float(item.get("elapsed_seconds", 0)) >= tail_start
    ][-contract["tail_intervals"] :]
    tail_elapsed = [float(item.get("elapsed_seconds", 0)) for item in complete_tail]
    cadence_gaps = [
        current - previous for previous, current in zip(tail_elapsed, tail_elapsed[1:])
    ]
    interval_seconds = contract["observation_interval_seconds"]
    tail_coverage = (
        tail_elapsed[-1] - tail_elapsed[0] + interval_seconds if tail_elapsed else 0.0
    )
    cadence_ok = (
        len(tail_elapsed) == contract["tail_intervals"]
        and all(
            interval_seconds * 0.5 <= gap <= interval_seconds * 1.5
            for gap in cadence_gaps
        )
        and tail_coverage >= contract["tail_intervals"] * interval_seconds * 0.9
        and final_elapsed - tail_elapsed[-1] <= interval_seconds * 1.5
    )
    tail_signals = [
        item
        for item, raw in zip(extracted, valid, strict=True)
        if float(raw.get("elapsed_seconds", 0)) >= tail_start
    ]
    rss_peak, rss_growth = rss(resources, tail_start, ceilings)
    replay_gap, expired_cursor, replay_anchor = replay(replay_probe, requirements)
    shutdown, cleanup, inventory, source_stable = lifecycle(
        lifecycle_evidence, base_passed
    )
    used_rows = [item["used_rows"] for item in extracted]
    used_bytes = [item["used_bytes"] for item in extracted]
    expired_rows = [item["expired_physical_rows"] for item in extracted]
    relation = [item["prefix_relation_bytes"] for item in extracted]
    event_relation = [item["run_events_relation_bytes"] for item in tail_signals]
    wal = [item["wal_bytes"] for item in extracted]
    relation_growth, wal_growth = growth(relation), growth(wal)
    successes = workload.get("successful_runs", 0)
    duration_ok = (
        isinstance(workload.get("elapsed_seconds"), (int, float))
        and workload["elapsed_seconds"] >= requirements["minimum_workload_seconds"]
    )
    stop_ok = (
        not requirements["require_duration_stop"]
        or workload.get("stop_reason") == "duration"
    )
    delete_indexes = [
        index
        for index in range(1, len(extracted))
        if extracted[index - 1]["aggregate_tuples_deleted"] is not None
        and extracted[index]["aggregate_tuples_deleted"] is not None
        and extracted[index]["aggregate_tuples_deleted"]
        > extracted[index - 1]["aggregate_tuples_deleted"]
    ]
    delete_deltas = [
        extracted[index]["aggregate_tuples_deleted"]
        - extracted[index - 1]["aggregate_tuples_deleted"]
        for index in delete_indexes
    ]
    criteria = {
        "journal_configuration": criterion(
            journal_configuration == contract["journal"],
            declared=contract["journal"],
            runtime=journal_configuration,
        ),
        "workload_duration": criterion(
            duration_ok and stop_ok,
            elapsed_seconds=workload.get("elapsed_seconds"),
            minimum_seconds=requirements["minimum_workload_seconds"],
            stop_reason=workload.get("stop_reason"),
            duration_stop_required=requirements["require_duration_stop"],
            attempted_runs=workload.get("attempted_runs"),
            requested_run_cap=workload.get("requested_run_cap"),
        ),
        "observation_intervals": criterion(
            len(valid) >= requirements["minimum_intervals"] and len(valid) == len(observations),
            observed=len(valid),
            total=len(observations),
            minimum=requirements["minimum_intervals"],
        ),
        "retained_rows": criterion(
            maximum(used_rows) is not None
            and maximum(used_rows) <= ceilings["retained_rows"],
            observed_max=maximum(used_rows),
            ceiling=ceilings["retained_rows"],
        ),
        "retained_bytes": criterion(
            maximum(used_bytes) is not None
            and maximum(used_bytes) <= ceilings["retained_bytes"],
            observed_max=maximum(used_bytes),
            ceiling=ceilings["retained_bytes"],
        ),
        "expired_physical_rows": criterion(
            maximum(expired_rows) is not None
            and maximum(expired_rows) <= ceilings["expired_physical_rows"],
            observed_max=maximum(expired_rows),
            ceiling=ceilings["expired_physical_rows"],
        ),
        "configured_prune_batch": criterion(
            journal_configuration.get("prune_batch") == contract["journal"]["prune_batch"],
            configured=journal_configuration.get("prune_batch"),
            declared=contract["journal"]["prune_batch"],
            semantics="configuration bound; interval delete deltas are aggregate",
        ),
        "physical_prune_progress": criterion(
            len(prune_indexes) >= requirements["minimum_prune_intervals"]
            and len(delete_indexes) >= requirements["minimum_prune_intervals"],
            retention_state_intervals=len(prune_indexes),
            pg_stat_delete_intervals=len(delete_indexes),
            minimum=requirements["minimum_prune_intervals"],
            retention_state_dropped_sequence_deltas=[
                extracted[index]["retention_dropped_sequences"]
                - extracted[index - 1]["retention_dropped_sequences"]
                for index in prune_indexes
            ],
            aggregate_pg_stat_delete_deltas=delete_deltas,
        ),
        "post_boundary_samples": criterion(
            post_boundary >= requirements["minimum_post_boundary_intervals"],
            observed=post_boundary,
            minimum=requirements["minimum_post_boundary_intervals"],
            boundary_interval=boundary,
        ),
        "post_prune_growth": criterion(
            boundary is not None
            and growth(used_rows, boundary) is not None
            and growth(used_bytes, boundary) is not None
            and growth(used_rows, boundary) <= ceilings["post_prune_growth_rows"]
            and growth(used_bytes, boundary) <= ceilings["post_prune_growth_bytes"],
            rows=growth(used_rows, boundary) if boundary is not None else None,
            row_ceiling=ceilings["post_prune_growth_rows"],
            bytes=growth(used_bytes, boundary) if boundary is not None else None,
            byte_ceiling=ceilings["post_prune_growth_bytes"],
        ),
        "readiness_failures": health(
            observations, "health_readiness_probe", ceilings["readiness_failures"]
        ),
        "liveness_failures": health(
            observations, "health_liveness_probe", ceilings["liveness_failures"]
        ),
        "rss_peak": rss_peak,
        "tail_rss_growth": rss_growth,
        "resource_sampler_stopped": criterion(
            resources.get("sampler_thread_stopped") is True,
            observed=resources.get("sampler_thread_stopped"),
        ),
        "tail_latency": latency(
            complete_tail, ceilings["tail_latency_ms"], contract["tail_intervals"]
        ),
        "tail_cadence": criterion(
            cadence_ok,
            interval_seconds=interval_seconds,
            elapsed_seconds=tail_elapsed,
            gaps_seconds=cadence_gaps,
            coverage_seconds=tail_coverage,
            final_sample_lag_seconds=(
                final_elapsed - tail_elapsed[-1] if tail_elapsed else None
            ),
        ),
        "tail_run_events_relation_growth": criterion(
            growth(event_relation) is not None
            and growth(event_relation) <= ceilings["tail_run_events_relation_growth_bytes"],
            observed=growth(event_relation),
            ceiling=ceilings["tail_run_events_relation_growth_bytes"],
            tail_start_seconds=tail_start,
        ),
        "prefix_relation": criterion(
            maximum(relation) is not None
            and relation_growth is not None
            and isinstance(successes, int)
            and successes > 0
            and maximum(relation) <= ceilings["prefix_relation_bytes"]
            and relation_growth / successes <= ceilings["prefix_relation_bytes_per_success"],
            observed_max=maximum(relation),
            ceiling=ceilings["prefix_relation_bytes"],
            growth_bytes=relation_growth,
            successful_runs=successes,
            growth_bytes_per_success=(relation_growth / successes if successes else None),
            per_success_ceiling=ceilings["prefix_relation_bytes_per_success"],
        ),
        "wal": criterion(
            wal_growth is not None
            and isinstance(successes, int)
            and successes > 0
            and wal_growth <= ceilings["wal_bytes"]
            and wal_growth / successes <= ceilings["wal_bytes_per_success"],
            observed=wal_growth,
            ceiling=ceilings["wal_bytes"],
            successful_runs=successes,
            bytes_per_success=(wal_growth / successes if successes else None),
            per_success_ceiling=ceilings["wal_bytes_per_success"],
            attribution="database-wide; use an isolated database",
        ),
        "explicit_replay_gap": replay_gap,
        "expired_cursor": expired_cursor,
        "replay_anchor": replay_anchor,
        "graceful_shutdown": shutdown,
        "cleanup": cleanup,
        "post_cleanup_inventory": inventory,
        "source_stable": source_stable,
    }
    stable_names = (
        "post_prune_growth",
        "post_boundary_samples",
        "tail_rss_growth",
        "tail_latency",
        "tail_cadence",
        "tail_run_events_relation_growth",
    )
    criteria["stable_tail"] = criterion(
        all(criteria[name]["passed"] for name in stable_names),
        components=list(stable_names),
    )
    criteria["overall_passed"] = all(item["passed"] for item in criteria.values())
    return criteria
