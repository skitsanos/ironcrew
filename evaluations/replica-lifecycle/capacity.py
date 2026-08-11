#!/usr/bin/env python3
"""IC-020 local 1->2->3 replica resource and concurrency gate."""

from __future__ import annotations

import argparse
import platform
import secrets
import tempfile
import urllib.parse
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from capacity_assertions import validate_journal, validate_process_metrics
from capacity_config import (
    ACTIVE_RUNS_PER_REPLICA,
    DB_POOL_PER_REPLICA,
    EVENT_MAX_BYTES,
    EVENT_PAYLOAD_ENVELOPE_PER_PROCESS,
    HERE,
    JOURNAL_MAX_TOTAL_BYTES,
    JOURNAL_MAX_TOTAL_EVENTS,
    MAX_EVENTS_PER_RUN,
    REPLAY_BYTES_PER_RUN,
    REPLICA_COUNTS,
    ROOT,
    RSS_PER_PROCESS_BYTES,
    SSE_PER_REPLICA,
    child_environment,
    container_contract,
    git_revision,
    sha256,
)
from harness_runtime import (
    ReplicaSet,
    SseHandle,
    extra_sse_status,
    replica_metrics,
    sample_rss,
)
from mock_provider import ProviderFixture
from phase_control import submit_runs, wait_quiescent, wait_terminal
from postgres_observer import PostgresObserver, safe_database_label
from reporting import sanitize_failure, write_report


def run_phase(
    count: int,
    replicas: ReplicaSet,
    provider: Any,
    observer: PostgresObserver,
    prefix: str,
    token: str,
) -> dict[str, Any]:
    planned = count * ACTIVE_RUNS_PER_REPLICA
    baseline = sample_rss(replicas.processes, samples=3)
    provider.gate.begin(f"replicas-{count}", planned)
    streams: list[SseHandle] = []
    try:
        runs = submit_runs(replicas, token, count)
        provider.gate.wait_saturated(15)
        for name, run_ids in runs.items():
            for run_id in run_ids:
                stream = SseHandle(
                    f"{replicas.bases[name]}/flows/capacity/events/{run_id}", token
                )
                stream.start()
                streams.append(stream)

        rejected = 0
        metrics = {}
        for name, run_ids in runs.items():
            status = extra_sse_status(
                f"{replicas.bases[name]}/flows/capacity/events/{run_ids[0]}", token
            )
            rejected += int(status == 429)
            if status != 429:
                raise RuntimeError(f"extra SSE was not rejected on {name}: HTTP {status}")
            metrics[name] = replica_metrics(replicas.bases[name], token)
            validate_process_metrics(name, metrics[name])

        saturated_rss = sample_rss(replicas.processes)
        saturated_rss["per_process_ceiling_bytes"] = RSS_PER_PROCESS_BYTES
        saturated_rss["aggregate_ceiling_bytes"] = count * RSS_PER_PROCESS_BYTES
        if any(
            value >= RSS_PER_PROCESS_BYTES
            for value in saturated_rss["per_process_peak_bytes"].values()
        ):
            raise RuntimeError("per-process RSS comparator exceeded")
        if saturated_rss["aggregate_peak_bytes"] >= count * RSS_PER_PROCESS_BYTES:
            raise RuntimeError("aggregate RSS comparator exceeded")

        snapshots = [observer.snapshot(prefix) for _ in range(3)]
        connection_samples = [item["connections_excluding_observer"] for item in snapshots]
        if not count <= max(connection_samples) <= count * DB_POOL_PER_REPLICA:
            raise RuntimeError(f"PostgreSQL connection envelope failed: {connection_samples}")
        held = snapshots[-1]
        if held["runs"]["active"] != planned:
            raise RuntimeError(f"durable active-run count was not {planned}: {held['runs']}")
        validate_journal(held)
        provider_held = provider.gate.snapshot()
        if provider_held["peak_active_calls"] != planned or provider_held["arrivals"] != planned:
            raise RuntimeError(f"provider plan diverged: {provider_held}")
    finally:
        provider.gate.release()

    provider.gate.wait_idle(15)
    wait_terminal(replicas, token, runs)
    for stream in streams:
        stream.wait_closed()
    replicas.assert_alive()
    quiescence = wait_quiescent(replicas, token)
    after = observer.snapshot(prefix)
    validate_journal(after)
    provider_done = provider.gate.snapshot()
    if provider_done["failed_calls"] or provider_done["active_calls"]:
        raise RuntimeError(f"provider calls did not finish cleanly: {provider_done}")
    metric_values = list(metrics.values())
    return {
        "replicas": count,
        "rss_baseline": baseline,
        "rss_saturated": saturated_rss,
        "postgres_connections": {
            "samples": connection_samples,
            "peak": max(connection_samples),
            "ceiling": count * DB_POOL_PER_REPLICA,
        },
        "provider": {**provider_done, "planned_calls": planned, "live_provider": False},
        "postgres_pool_metrics": {
            "aggregate_open_connections": sum(
                item["ironcrew_postgres_pool_open_connections"] for item in metric_values
            ),
            "aggregate_in_use_connections": sum(
                item["ironcrew_postgres_pool_in_use_connections"] for item in metric_values
            ),
            "aggregate_limit": count * DB_POOL_PER_REPLICA,
        },
        "sse": {
            "established": len(streams),
            "aggregate_limit": count * SSE_PER_REPLICA,
            "extra_rejections": rejected,
            "metrics_by_replica": metrics,
        },
        "event_buffers": {
            "measurement": "approximate_retained_serialization_plus_configured_capacity",
            "observed_retained_events": sum(
                item["ironcrew_process_eventbus_retained_events"] for item in metric_values
            ),
            "observed_retained_bytes": sum(
                item["ironcrew_process_eventbus_retained_bytes"] for item in metric_values
            ),
            "retained_events_capacity": count * ACTIVE_RUNS_PER_REPLICA * MAX_EVENTS_PER_RUN,
            "retained_bytes_capacity": count * ACTIVE_RUNS_PER_REPLICA * REPLAY_BYTES_PER_RUN,
            "per_process_bytes": EVENT_PAYLOAD_ENVELOPE_PER_PROCESS,
            "aggregate_bytes": count * EVENT_PAYLOAD_ENVELOPE_PER_PROCESS,
            "allocator_or_rss_measurement": False,
        },
        "process_metrics_by_replica": metrics,
        "postgres_while_saturated": held,
        "postgres_after_completion": after,
        "post_phase_quiescence": quiescence,
    }

def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--database-url", required=True)
    parser.add_argument("--postgres-container", required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    parsed = urllib.parse.urlsplit(args.database_url)
    if parsed.hostname not in {"127.0.0.1", "localhost", "::1"}:
        parser.error("this destructive local gate requires a loopback PostgreSQL URL")
    binary = args.binary.resolve()
    if not binary.is_file():
        parser.error("--binary must identify an existing file")

    started = datetime.now(UTC)
    commit, dirty = git_revision()
    prefix = f"ic020cap_{secrets.token_hex(4)}_"
    token = secrets.token_urlsafe(32)
    observer = PostgresObserver(args.database_url, args.postgres_container)
    report: dict[str, Any] = {
        "schema": "ironcrew.replica-lifecycle-capacity.v1",
        "date": started.date().isoformat(),
        "started_at": started.isoformat(),
        "status": "failed",
        "summary": "The local capacity gate did not complete.",
        "evidence_boundary": "local_process_postgresql15_loopback_mock",
        "revision": {
            "git_commit": commit,
            "dirty": dirty,
            "binary_sha256": sha256(binary),
        },
        "host": {"platform": platform.platform(), "python": platform.python_version()},
        "database": safe_database_label(args.database_url),
        "container_image": container_contract(args.postgres_container),
        "table_prefix": prefix,
        "limits": {
            "replica_counts": list(REPLICA_COUNTS),
            "db_pool_per_replica": DB_POOL_PER_REPLICA,
            "active_runs_per_replica": ACTIVE_RUNS_PER_REPLICA,
            "sse_per_replica": SSE_PER_REPLICA,
            "rss_per_process_bytes": RSS_PER_PROCESS_BYTES,
            "max_events_per_run": MAX_EVENTS_PER_RUN,
            "event_replay_bytes_per_run": REPLAY_BYTES_PER_RUN,
            "event_max_bytes": EVENT_MAX_BYTES,
            "journal_max_total_events": JOURNAL_MAX_TOTAL_EVENTS,
            "journal_max_total_bytes": JOURNAL_MAX_TOTAL_BYTES,
            "event_payload_envelope_per_process_bytes": EVENT_PAYLOAD_ENVELOPE_PER_PROCESS,
        },
        "phases": [],
        "cleanup": None,
        "shutdown": None,
        "error": None,
    }
    replicas: ReplicaSet | None = None
    provider: Any | None = None
    with tempfile.TemporaryDirectory(prefix="ironcrew-ic020-capacity-") as temp:
        try:
            with ProviderFixture() as provider:
                replicas = ReplicaSet(ROOT, binary, HERE / "flows", Path(temp))
                for count in REPLICA_COUNTS:
                    name = f"replica-{count}"
                    replicas.start(
                        name,
                        child_environment(args.database_url, prefix, name, token, provider.base_url),
                    )
                    phase = run_phase(count, replicas, provider, observer, prefix, token)
                    report["phases"].append(phase)
                    report["postgres_server_version"] = phase["postgres_after_completion"][
                        "server_version"
                    ]
            report["summary"] = (
                "One, two, and three real IronCrew processes stayed within the predeclared "
                "local envelopes while provider work and SSE were saturated."
            )
        except BaseException as error:  # preserve cleanup evidence before returning failure
            password = urllib.parse.unquote(parsed.password or "")
            report["error"] = sanitize_failure(
                str(error),
                database_url=args.database_url,
                secret_canaries=(token, password),
            )
        finally:
            if provider is not None:
                provider.gate.release()
            if replicas is not None:
                report["shutdown"] = replicas.stop_all()
            try:
                report["cleanup"] = observer.cleanup(prefix)
            except BaseException as cleanup_error:
                report["error"] = report["error"] or sanitize_failure(
                    f"cleanup failed: {cleanup_error}",
                    database_url=args.database_url,
                    secret_canaries=(token, urllib.parse.unquote(parsed.password or "")),
                )

    clean_shutdown = bool(report["shutdown"]) and all(
        item == {"exit_code": 0, "forced_kill": False}
        for item in report["shutdown"].values()
    )
    clean_prefix = report["cleanup"] is not None
    if len(report["phases"]) == len(REPLICA_COUNTS) and clean_shutdown and clean_prefix and not report["error"]:
        report["status"] = "passed"
    report["finished_at"] = datetime.now(UTC).isoformat()
    json_path, markdown_path = write_report(args.report, report)
    print(json_path)
    print(markdown_path)
    print(report["status"])
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
