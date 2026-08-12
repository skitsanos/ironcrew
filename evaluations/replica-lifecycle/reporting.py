"""Stable JSON and Markdown output for the IC-020 capacity gate."""

from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any


POSTGRES_DSN = re.compile(r"postgres(?:ql)?://[^\s`'\"<>]+")


def sanitize_failure(
    message: str,
    *,
    database_url: str,
    secret_canaries: tuple[str, ...] = (),
    limit: int = 1000,
) -> str:
    """Redact credential-bearing DSNs and high-entropy fixture canaries."""
    redacted = message.replace(database_url, "<redacted-dsn>")
    redacted = POSTGRES_DSN.sub("<redacted-dsn>", redacted)
    for canary in secret_canaries:
        if len(canary) >= 16:
            redacted = redacted.replace(canary, "<redacted-secret>")
    return redacted[:limit]


def mib(value: int | None) -> str:
    if value is None:
        return "n/a"
    return f"{value / (1024 * 1024):.2f} MiB"


def markdown(report: dict[str, Any]) -> str:
    lines = [
        f"# IC-020 local replica-capacity evidence — {report['date']}",
        "",
        f"**Result: {report['status'].upper()}**",
        "",
        report["summary"],
        "",
        "## Evidence boundary",
        "",
        "This is a local macOS/Linux host-process and disposable PostgreSQL 15 gate. "
        "The provider is a bounded loopback mock. RSS is sampled from host processes, "
        "not a pod cgroup, and this report is not Railway/OpenShift or live-provider proof.",
        "",
        "## Predeclared ceilings",
        "",
        "| Resource | Per replica | Aggregate at R replicas |",
        "|---|---:|---:|",
        f"| PostgreSQL pool | {report['limits']['db_pool_per_replica']} | `R × {report['limits']['db_pool_per_replica']}` |",
        f"| Active runs / planned provider calls | {report['limits']['active_runs_per_replica']} | `R × {report['limits']['active_runs_per_replica']}` |",
        f"| Live SSE | {report['limits']['sse_per_replica']} | `R × {report['limits']['sse_per_replica']}` |",
        f"| Host RSS comparator | {mib(report['limits']['rss_per_process_bytes'])} | `R × {mib(report['limits']['rss_per_process_bytes'])}` |",
        f"| Replay + durable-queue logical payload envelope | {mib(report['limits']['event_payload_envelope_per_process_bytes'])} | `R × {mib(report['limits']['event_payload_envelope_per_process_bytes'])}` |",
        "",
        "EventBus retained bytes are measured as approximate serialized payload size; "
        "capacity is configured logical payload capacity, not heap/RSS. Both exclude "
        "Rust metadata, and the broadcast ring shares `Arc` payloads with replay history. "
        "PostgreSQL journal bytes are measured independently.",
        "",
        "## Phase results",
        "",
        "| R | PG conns peak / ceiling | Provider peak / plan | SSE open / ceiling | Extra SSE rejects | EventBus retained / capacity | RSS peak / comparator | Journal rows / accounted bytes |",
        "|---:|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for phase in report.get("phases", []):
        journal = phase["postgres_after_completion"]["journal"]
        lines.append(
            "| {replicas} | {pg_peak} / {pg_limit} | {provider_peak} / {provider_plan} | "
            "{sse_open} / {sse_limit} | {sse_rejects} / {replicas} | {event_bytes} / {event_capacity} bytes | {rss} / {rss_limit} | "
            "{rows} / {bytes} |".format(
                replicas=phase["replicas"],
                pg_peak=phase["postgres_connections"]["peak"],
                pg_limit=phase["postgres_connections"]["ceiling"],
                provider_peak=phase["provider"]["peak_active_calls"],
                provider_plan=phase["provider"]["planned_calls"],
                sse_open=phase["sse"]["established"],
                sse_limit=phase["sse"]["aggregate_limit"],
                sse_rejects=phase["sse"]["extra_rejections"],
                event_bytes=phase["event_buffers"]["observed_retained_bytes"],
                event_capacity=phase["event_buffers"]["retained_bytes_capacity"],
                rss=mib(phase["rss_saturated"]["aggregate_peak_bytes"]),
                rss_limit=mib(phase["rss_saturated"]["aggregate_ceiling_bytes"]),
                rows=journal["actual_rows"],
                bytes=journal["accounted_bytes"],
            )
        )
    lines.extend(
        [
            "",
            "Each phase held exactly two provider calls and two SSE streams per process "
            "while sampling. Every process reported its local active-run/SSE gauges at "
            "the configured limit; one additional direct SSE request per process returned 429.",
            "",
            "## Bounded post-phase quiescence",
            "",
            "Before scaling to the next process count, the gate closed every SSE stream and "
            "boundedly waited for active runs, provider calls, SSE connections, EventBus "
            "instances, retained events/bytes, and their configured capacities to reach zero.",
            "",
            "| R | Cleanup latency | Replicas checked | Exact zero snapshot |",
            "|---:|---:|---:|---:|",
        ]
    )
    for phase in report.get("phases", []):
        quiescence = phase["post_phase_quiescence"]
        metric_names = quiescence["required_zero_metrics"]
        snapshots = quiescence["metrics_by_replica"]
        exact_zero = all(
            metrics[name] == 0
            for metrics in snapshots.values()
            for name in metric_names
        )
        lines.append(
            f"| {phase['replicas']} | {quiescence['elapsed_ms']:.3f} ms | "
            f"{len(snapshots)} | `{str(exact_zero).lower()}` |"
        )
    lines.extend(
        [
            "",
            "## Reproducibility and cleanup",
            "",
            f"- Git commit: `{report['revision']['git_commit']}` (dirty worktree: `{str(report['revision']['dirty']).lower()}`)",
            f"- Binary SHA-256: `{report['revision']['binary_sha256']}`",
            f"- PostgreSQL: `{report.get('postgres_server_version', 'unknown')}` using the moving `postgres:15` contract",
            f"- Exact prefix cleanup: `{json.dumps(report.get('cleanup'), sort_keys=True)}`",
            f"- Controlled replica exits: `{json.dumps(report.get('shutdown'), sort_keys=True)}`",
        ]
    )
    if report.get("error"):
        lines.extend(["", "## Failure", "", f"`{report['error']}`"])
    return "\n".join(lines) + "\n"


def write_report(path: Path, report: dict[str, Any]) -> tuple[Path, Path]:
    path.parent.mkdir(parents=True, exist_ok=True)
    json_path = path.with_suffix(".json")
    markdown_path = path.with_suffix(".md")
    json_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    markdown_path.write_text(markdown(report), encoding="utf-8")
    return json_path, markdown_path
