"""Workload aggregation and legacy pass criteria for replica soak."""

from __future__ import annotations

import math
from typing import Any

from replica_topology import host_rss_pass_criterion, topology_pass_criterion


def _choose_details(
    results: list[dict[str, Any]], maximum: int
) -> list[dict[str, Any]]:
    if maximum == 0:
        return []
    failures = [result for result in results if not result.get("success")]
    successes = [result for result in results if result.get("success")]
    return [
        {key: value for key, value in result.items() if not key.startswith("_")}
        for result in (failures + successes)[:maximum]
    ]


def aggregate_workload(
    results: list[dict[str, Any]], metrics: Any, args: Any, stop_reason: str
) -> dict[str, Any]:
    successes = [result for result in results if result.get("success")]
    failures = [result for result in results if not result.get("success")]
    pending_ms = sum(float(result.get("pending_ms", 0.0)) for result in successes)
    sse_latency_ms = metrics.total_latency_ms("sse_initial") + metrics.total_latency_ms(
        "sse_reconnect"
    )
    return {
        "attempted_runs": len(results),
        "successful_runs": len(successes),
        "failed_runs": len(failures),
        "requested_run_cap": args.runs,
        "duration_cap_seconds": args.duration_seconds,
        "stop_reason": stop_reason,
        "operations": metrics.report(),
        "polling_pressure": {
            "measured_client": {
                "question_http_polls": metrics.count("questions_poll"),
                "sse_initial_connections": metrics.count("sse_initial"),
                "sse_reconnections": metrics.count("sse_reconnect"),
                "peer_answer_requests": metrics.count("answer_peer"),
            },
            "derived_server_opportunities": {
                "owner_hitl_reads_upper_estimate": math.ceil(
                    pending_ms / max(args.hitl_poll_ms, 1)
                ),
                "journal_sse_poll_upper_estimate": math.ceil(
                    sse_latency_ms / max(args.journal_poll_ms, 1)
                ),
            },
            "semantics": {
                "measured_client": "exact requests/connections issued by this runner",
                "derived_server_opportunities": (
                    "latency divided by configured poll interval; not a database call count"
                ),
                "postgres_measured_calls": (
                    "reported separately only when pg_stat_statements is installed and readable"
                ),
            },
        },
        "result_details": _choose_details(results, args.max_run_details),
        "result_details_truncated": len(results) > args.max_run_details,
    }


def build_pass_criteria(
    report: dict[str, Any], metrics: Any, args: Any, launcher: Any
) -> dict[str, Any]:
    workload = report.get("workload", {})
    postgres = report.get("postgres", {}).get("delta", {})
    resources = report.get("resources", {}).get("replicas", {})
    workload_ok = workload.get("attempted_runs", 0) > 0 and workload.get("failed_runs") == 0
    operation_report = metrics.report()
    liveness_errors = operation_report.get("health_liveness_probe", {}).get("errors", 0)
    readiness_errors = operation_report.get("health_readiness_probe", {}).get("errors", 0)
    health_errors = liveness_errors + readiness_errors
    deadlocks = postgres.get("database_activity", {}).get("deadlocks")
    stats_stable = postgres.get("stats_reset_changed") is False
    if args.mode == "launch":
        replica_states = {
            name: process.poll() is None for name, process in launcher.processes.items()
        }
        replicas_alive = len(replica_states) == 2 and all(replica_states.values())
    else:
        replica_states, replicas_alive = {}, True
    criteria = {
        "workload": {
            "passed": workload_ok,
            "attempted_runs": workload.get("attempted_runs", 0),
            "failed_runs": workload.get("failed_runs"),
        },
        "health_probe_errors": {
            "passed": health_errors == 0,
            "errors": health_errors,
            "liveness_errors": liveness_errors,
            "readiness_errors": readiness_errors,
        },
        "replicas_alive_before_shutdown": {
            "status": (
                "passed"
                if args.mode == "launch" and replicas_alive
                else "failed"
                if args.mode == "launch"
                else "not_applicable"
            ),
            "passed": replicas_alive if args.mode == "launch" else None,
            "applicable": args.mode == "launch",
            "states": replica_states,
        },
        "replica_instance_count": topology_pass_criterion(report.get("replica_topology")),
        "postgres_deadlocks": {"passed": deadlocks == 0, "delta": deadlocks},
        "postgres_stats_reset_stable": {"passed": stats_stable},
        "host_process_rss_comparator": host_rss_pass_criterion(
            resources, args.memory_comparator_mib * 1024 * 1024
        ),
    }
    criteria["overall_passed"] = all(
        value.get("passed") is True
        for value in criteria.values()
        if isinstance(value, dict) and value.get("applicable", True)
    )
    return criteria
