#!/usr/bin/env python3
"""Bounded two-replica IronCrew/PostgreSQL soak evaluator.

The runner uses only Python's standard library. PostgreSQL observations are
collected through `psql`, either on the host or through `docker exec`.
"""

from __future__ import annotations

import argparse
import base64
import concurrent.futures
import hashlib
import json
import math
import os
import platform
import random
import re
import secrets
import shutil
import signal
import statistics
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from collections import Counter
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any


SCHEMA_VERSION = "ironcrew.replica-soak.v1"
ROOT = Path(__file__).resolve().parents[2]
HERE = Path(__file__).resolve().parent
FLOW_ROOT = HERE / "flows"
REPORT_ROOT = HERE / "reports"
TABLE_SUFFIXES = (
    "runs",
    "conversations",
    "dialogs",
    "audit_events",
    "idempotency",
    "idempotency_accounting",
    "human_inputs",
    "run_events",
    "run_event_state",
    "run_event_usage",
)
PREFIX_PATTERN = re.compile(r"[a-z][a-z0-9_]{2,31}")
MAX_LATENCY_SAMPLES = 10_000
MAX_RESPONSE_BYTES = 1024 * 1024
MAX_RUN_DETAILS_HARD = 1_000


def utc_now() -> str:
    return datetime.now(UTC).isoformat()


def percentile(sorted_values: list[float], quantile: float) -> float | None:
    if not sorted_values:
        return None
    index = max(0, math.ceil(quantile * len(sorted_values)) - 1)
    return round(sorted_values[index], 3)


def sanitize_error(error: BaseException | str, secrets_to_remove: tuple[str, ...] = ()) -> str:
    message = str(error).replace("\n", " ").replace("\r", " ")
    for value in secrets_to_remove:
        if value:
            message = message.replace(value, "<redacted>")
    return message[:500]


def safe_database_label(database_url: str) -> str:
    parsed = urllib.parse.urlsplit(database_url)
    host = parsed.hostname or "unknown"
    port = f":{parsed.port}" if parsed.port else ""
    database = parsed.path.lstrip("/") or "unknown"
    return f"{parsed.scheme}://{host}{port}/{database}"


def validate_prefix(prefix: str) -> str:
    if not PREFIX_PATTERN.fullmatch(prefix):
        raise ValueError(
            "table prefix must be 3-32 lowercase ASCII alphanumeric/underscore bytes "
            "and start with a letter"
        )
    return prefix


def sse_event_payload(event: dict[str, Any]) -> Any:
    """Return the CrewEvent data object from an SSE parser result.

    Durable IronCrew events use the same tagged `{"event", "data"}` JSON
    envelope as live events. Synthetic terminal recovery uses that envelope as
    well, while this helper also tolerates a future bare-data representation.
    """
    payload = event.get("data")
    if (
        isinstance(payload, dict)
        and payload.get("event") == event.get("event")
        and "data" in payload
    ):
        return payload["data"]
    return payload


class OperationMetrics:
    """Thread-safe bounded latency/error accumulator with reservoir sampling."""

    def __init__(self, max_samples: int = MAX_LATENCY_SAMPLES) -> None:
        self._lock = threading.Lock()
        self._max_samples = max_samples
        self._rng = random.Random(0x1A0C0E)
        self._operations: dict[str, dict[str, Any]] = {}

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
            entry = self._operations.setdefault(
                operation,
                {
                    "count": 0,
                    "ok": 0,
                    "errors": 0,
                    "sum_latency_ms": 0.0,
                    "max_latency_ms": 0.0,
                    "response_bytes": 0,
                    "status_counts": Counter(),
                    "error_kinds": Counter(),
                    "samples": [],
                },
            )
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

    def count(self, operation: str) -> int:
        with self._lock:
            return int(self._operations.get(operation, {}).get("count", 0))

    def total_latency_ms(self, operation: str) -> float:
        with self._lock:
            return float(self._operations.get(operation, {}).get("sum_latency_ms", 0.0))

    def report(self) -> dict[str, Any]:
        output: dict[str, Any] = {}
        with self._lock:
            snapshot = list(self._operations.items())
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


@dataclass
class HttpResult:
    status: int
    body: bytes
    headers: dict[str, str]
    latency_ms: float

    def json(self) -> Any:
        return json.loads(self.body)


class HttpClient:
    def __init__(self, token: str | None, timeout: float, metrics: OperationMetrics) -> None:
        self.token = token
        self.timeout = timeout
        self.metrics = metrics

    def headers(self, extra: dict[str, str] | None = None) -> dict[str, str]:
        headers = {"Accept": "application/json", "User-Agent": "ironcrew-replica-soak/1"}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        if extra:
            headers.update(extra)
        return headers

    def request(
        self,
        operation: str,
        method: str,
        url: str,
        payload: Any | None = None,
        headers: dict[str, str] | None = None,
    ) -> HttpResult:
        body = None
        request_headers = self.headers(headers)
        if payload is not None:
            body = json.dumps(payload, separators=(",", ":")).encode()
            request_headers["Content-Type"] = "application/json"
        request = urllib.request.Request(url, data=body, headers=request_headers, method=method)
        started = time.perf_counter()
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                response_body = response.read(MAX_RESPONSE_BYTES + 1)
                if len(response_body) > MAX_RESPONSE_BYTES:
                    raise RuntimeError("HTTP response exceeded one MiB")
                status = response.status
                latency_ms = (time.perf_counter() - started) * 1000
                self.metrics.record(
                    operation,
                    latency_ms,
                    status,
                    200 <= status < 400,
                    response_bytes=len(response_body),
                )
                return HttpResult(status, response_body, dict(response.headers), latency_ms)
        except urllib.error.HTTPError as error:
            response_body = error.read(MAX_RESPONSE_BYTES + 1)
            latency_ms = (time.perf_counter() - started) * 1000
            self.metrics.record(
                operation,
                latency_ms,
                error.code,
                False,
                f"http_{error.code}",
                len(response_body),
            )
            return HttpResult(error.code, response_body, dict(error.headers), latency_ms)
        except Exception as error:
            latency_ms = (time.perf_counter() - started) * 1000
            self.metrics.record(
                operation,
                latency_ms,
                None,
                False,
                type(error).__name__,
            )
            raise

    def sse_until(
        self,
        operation: str,
        url: str,
        expected_event: str,
        last_event_id: str | None = None,
        max_bytes: int = MAX_RESPONSE_BYTES,
    ) -> dict[str, Any]:
        headers = self.headers({"Accept": "text/event-stream"})
        if last_event_id:
            headers["Last-Event-ID"] = last_event_id
        request = urllib.request.Request(url, headers=headers, method="GET")
        started = time.perf_counter()
        deadline = time.monotonic() + self.timeout
        total_bytes = 0
        current_event = "message"
        current_id: str | None = None
        data_lines: list[str] = []
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                for raw_line in response:
                    if time.monotonic() >= deadline:
                        raise TimeoutError(f"SSE timed out before {expected_event}")
                    total_bytes += len(raw_line)
                    if total_bytes > max_bytes:
                        raise RuntimeError("SSE response exceeded configured byte limit")
                    line = raw_line.decode("utf-8", "replace").rstrip("\r\n")
                    if not line:
                        if current_event == expected_event:
                            latency_ms = (time.perf_counter() - started) * 1000
                            self.metrics.record(
                                operation,
                                latency_ms,
                                response.status,
                                True,
                                response_bytes=total_bytes,
                            )
                            data_text = "\n".join(data_lines)
                            try:
                                data = json.loads(data_text) if data_text else None
                            except json.JSONDecodeError:
                                data = data_text
                            return {
                                "event": current_event,
                                "id": current_id,
                                "data": data,
                                "bytes": total_bytes,
                                "latency_ms": latency_ms,
                            }
                        current_event = "message"
                        current_id = None
                        data_lines = []
                    elif line.startswith("event:"):
                        current_event = line[6:].strip()
                    elif line.startswith("id:"):
                        current_id = line[3:].strip()
                    elif line.startswith("data:"):
                        data_lines.append(line[5:].lstrip())
                raise RuntimeError(f"SSE closed before {expected_event}")
        except urllib.error.HTTPError as error:
            error.read(MAX_RESPONSE_BYTES)
            latency_ms = (time.perf_counter() - started) * 1000
            self.metrics.record(
                operation,
                latency_ms,
                error.code,
                False,
                f"http_{error.code}",
            )
            raise RuntimeError(f"SSE returned HTTP {error.code}") from error
        except Exception as error:
            latency_ms = (time.perf_counter() - started) * 1000
            self.metrics.record(
                operation,
                latency_ms,
                None,
                False,
                type(error).__name__,
                response_bytes=total_bytes,
            )
            raise


def read_int_file(path: Path) -> int | str | None:
    try:
        value = path.read_text(encoding="utf-8").strip()
    except (OSError, UnicodeError):
        return None
    if value == "max":
        return "max"
    try:
        return int(value)
    except ValueError:
        return None


def process_memory(pid: int) -> dict[str, Any] | None:
    status_path = Path(f"/proc/{pid}/status")
    if status_path.exists():
        values: dict[str, int] = {}
        try:
            for line in status_path.read_text(encoding="utf-8").splitlines():
                if line.startswith(("VmRSS:", "VmHWM:")):
                    key, raw = line.split(":", 1)
                    values[key] = int(raw.strip().split()[0]) * 1024
        except (OSError, ValueError):
            return None
        return {
            "rss_bytes": values.get("VmRSS"),
            "native_peak_rss_bytes": values.get("VmHWM"),
            "source": "linux_proc_status",
        }
    try:
        result = subprocess.run(
            ["ps", "-o", "rss=", "-p", str(pid)],
            check=True,
            capture_output=True,
            text=True,
            timeout=2,
        )
        rss_bytes = int(result.stdout.strip()) * 1024
    except (OSError, ValueError, subprocess.SubprocessError):
        return None
    return {
        "rss_bytes": rss_bytes,
        "native_peak_rss_bytes": None,
        "source": "ps_rss_sample",
    }


def cgroup_memory(pid: int) -> dict[str, Any] | None:
    if platform.system() != "Linux":
        return None
    try:
        memberships = Path(f"/proc/{pid}/cgroup").read_text(encoding="utf-8").splitlines()
    except OSError:
        return None
    for membership in memberships:
        fields = membership.split(":", 2)
        if len(fields) == 3 and fields[0] == "0":
            relative = fields[2].lstrip("/")
            root = Path("/sys/fs/cgroup") / relative
            events: dict[str, int] = {}
            try:
                for line in (root / "memory.events").read_text(encoding="utf-8").splitlines():
                    key, raw = line.split()
                    events[key] = int(raw)
            except (OSError, ValueError):
                pass
            return {
                "version": 2,
                "path": f"/{relative}" if relative else "/",
                "current_bytes": read_int_file(root / "memory.current"),
                "native_peak_bytes": read_int_file(root / "memory.peak"),
                "limit_bytes": read_int_file(root / "memory.max"),
                "events": events,
            }
    for membership in memberships:
        fields = membership.split(":", 2)
        if len(fields) != 3 or "memory" not in fields[1].split(","):
            continue
        relative = fields[2].lstrip("/")
        root = Path("/sys/fs/cgroup/memory") / relative
        return {
            "version": 1,
            "path": f"/{relative}" if relative else "/",
            "current_bytes": read_int_file(root / "memory.usage_in_bytes"),
            "native_peak_bytes": read_int_file(root / "memory.max_usage_in_bytes"),
            "limit_bytes": read_int_file(root / "memory.limit_in_bytes"),
            "events": {},
        }
    return None


class ResourceSampler:
    def __init__(
        self,
        pids: dict[str, int | None],
        interval: float,
        comparator_bytes: int,
        max_timeline_samples: int = 4_000,
    ) -> None:
        self.pids = pids
        self.interval = interval
        self.comparator_bytes = comparator_bytes
        self.max_timeline_samples = max_timeline_samples
        self.stop_event = threading.Event()
        self.thread: threading.Thread | None = None
        self.started_monotonic = time.monotonic()
        self.samples: dict[str, dict[str, Any]] = {
            name: {
                "pid": pid,
                "samples": 0,
                "unavailable_samples": 0,
                "first_rss_bytes": None,
                "last_rss_bytes": None,
                "sampled_peak_rss_bytes": None,
                "native_peak_rss_bytes": None,
                "source": None,
                "timeline": [],
                "cgroup": None,
                "cgroup_sampled_peak_bytes": None,
            }
            for name, pid in pids.items()
        }

    def start(self) -> None:
        self.thread = threading.Thread(target=self._run, name="resource-sampler", daemon=True)
        self.thread.start()

    def stop(self) -> None:
        self.stop_event.set()
        if self.thread:
            self.thread.join(timeout=max(2.0, self.interval * 3))

    def _run(self) -> None:
        while not self.stop_event.is_set():
            elapsed = time.monotonic() - self.started_monotonic
            for name, pid in self.pids.items():
                target = self.samples[name]
                if pid is None:
                    target["unavailable_samples"] += 1
                    continue
                memory = process_memory(pid)
                if not memory or memory.get("rss_bytes") is None:
                    target["unavailable_samples"] += 1
                    continue
                rss = int(memory["rss_bytes"])
                target["samples"] += 1
                target["source"] = memory["source"]
                target["first_rss_bytes"] = (
                    rss if target["first_rss_bytes"] is None else target["first_rss_bytes"]
                )
                target["last_rss_bytes"] = rss
                target["sampled_peak_rss_bytes"] = max(
                    target["sampled_peak_rss_bytes"] or 0, rss
                )
                native_peak = memory.get("native_peak_rss_bytes")
                if isinstance(native_peak, int):
                    target["native_peak_rss_bytes"] = max(
                        target["native_peak_rss_bytes"] or 0, native_peak
                    )
                if len(target["timeline"]) < self.max_timeline_samples:
                    target["timeline"].append(
                        {"elapsed_s": round(elapsed, 3), "rss_bytes": rss}
                    )
                cgroup = cgroup_memory(pid)
                if cgroup:
                    target["cgroup"] = cgroup
                    current = cgroup.get("current_bytes")
                    if isinstance(current, int):
                        target["cgroup_sampled_peak_bytes"] = max(
                            target["cgroup_sampled_peak_bytes"] or 0, current
                        )
            self.stop_event.wait(self.interval)

    def report(self) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for name, raw in self.samples.items():
            first = raw["first_rss_bytes"]
            last = raw["last_rss_bytes"]
            peak = raw["sampled_peak_rss_bytes"]
            duration = max(time.monotonic() - self.started_monotonic, 0.001)
            result[name] = {
                **raw,
                "scope": "host_process_rss",
                "rss_delta_bytes": last - first
                if isinstance(first, int) and isinstance(last, int)
                else None,
                "endpoint_slope_bytes_per_second": round((last - first) / duration, 3)
                if isinstance(first, int) and isinstance(last, int)
                else None,
                "configured_memory_comparator_bytes": self.comparator_bytes,
                "sampled_peak_percent_of_comparator": round(
                    100 * peak / self.comparator_bytes, 3
                )
                if isinstance(peak, int) and self.comparator_bytes
                else None,
                "comparator_enforced_by_runner": False,
                "peak_semantics": (
                    "native VmHWM plus sampled RSS"
                    if raw["source"] == "linux_proc_status"
                    else "sampled RSS only; macOS exposes no live per-child native peak here"
                ),
            }
        cgroup_paths = {
            value["cgroup"]["path"]
            for value in result.values()
            if isinstance(value.get("cgroup"), dict)
        }
        return {
            "replicas": result,
            "cgroup_scope": {
                "distinct_paths": sorted(cgroup_paths),
                "shared_between_replicas": len(cgroup_paths) == 1 and len(result) == 2,
                "warning": (
                    "cgroup values include every process in that cgroup; they are not per-process RSS"
                ),
            },
        }


class PostgresClient:
    def __init__(
        self,
        database_url: str,
        psql_command: str | None,
        docker_container: str | None,
        timeout: float = 30.0,
    ) -> None:
        parsed = urllib.parse.urlsplit(database_url)
        if parsed.scheme not in {"postgres", "postgresql"}:
            raise ValueError("DATABASE_URL must use postgres:// or postgresql://")
        self.database_url = database_url
        self.parsed = parsed
        self.timeout = timeout
        self.docker_container = docker_container
        self.psql = psql_command or shutil.which("psql")
        if not self.docker_container and not self.psql:
            raise RuntimeError(
                "PostgreSQL metrics require psql on PATH or --postgres-container"
            )

    def _command(self) -> tuple[list[str], dict[str, str]]:
        user = urllib.parse.unquote(self.parsed.username or "postgres")
        database = urllib.parse.unquote(self.parsed.path.lstrip("/") or user)
        if self.docker_container:
            return (
                [
                    "docker",
                    "exec",
                    "-i",
                    self.docker_container,
                    "psql",
                    "--no-psqlrc",
                    "--tuples-only",
                    "--no-align",
                    "--set",
                    "ON_ERROR_STOP=1",
                    "--username",
                    user,
                    "--dbname",
                    database,
                    "--file",
                    "-",
                ],
                os.environ.copy(),
            )
        assert self.psql is not None
        environment = os.environ.copy()
        environment.update(
            {
                "PGHOST": self.parsed.hostname or "127.0.0.1",
                "PGPORT": str(self.parsed.port or 5432),
                "PGUSER": user,
                "PGDATABASE": database,
                "PGPASSWORD": urllib.parse.unquote(self.parsed.password or ""),
            }
        )
        query = urllib.parse.parse_qs(self.parsed.query)
        if query.get("sslmode"):
            environment["PGSSLMODE"] = query["sslmode"][-1]
        return (
            [
                self.psql,
                "--no-psqlrc",
                "--tuples-only",
                "--no-align",
                "--set",
                "ON_ERROR_STOP=1",
                "--file",
                "-",
            ],
            environment,
        )

    def execute(self, sql: str) -> str:
        command, environment = self._command()
        redaction_values = (
            self.database_url,
            urllib.parse.unquote(self.parsed.password or ""),
        )
        try:
            result = subprocess.run(
                command,
                input=sql,
                capture_output=True,
                text=True,
                env=environment,
                timeout=self.timeout,
                check=True,
            )
        except subprocess.CalledProcessError as error:
            detail = sanitize_error(
                error.stderr or error.stdout or error,
                redaction_values,
            )
            raise RuntimeError(f"PostgreSQL observation failed: {detail}") from error
        except (subprocess.TimeoutExpired, OSError) as error:
            detail = sanitize_error(error, redaction_values)
            raise RuntimeError(f"PostgreSQL observation failed: {detail}") from error
        # PostgreSQL's JSON serialization of aggregate records can contain
        # embedded newlines. Returning only the final psql output line corrupts
        # otherwise valid snapshots, so preserve the complete scalar value.
        return result.stdout.strip()

    def json(self, sql: str) -> Any:
        output = self.execute(sql)
        if not output:
            return None
        return json.loads(output)


def postgres_snapshot_sql(prefix: str) -> str:
    validate_prefix(prefix)
    tables = [f"{prefix}{suffix}" for suffix in TABLE_SUFFIXES]
    targets = ",".join(f"('{table}')" for table in tables)
    row_counts = " UNION ALL ".join(
        f"SELECT '{table}'::text AS relname, COUNT(*)::bigint AS exact_rows FROM {table}"
        for table in tables
    )
    events = f"{prefix}run_events"
    usage = f"{prefix}run_event_usage"
    human = f"{prefix}human_inputs"
    return f"""
WITH target(relname) AS (VALUES {targets}),
row_counts AS ({row_counts}),
table_metrics AS (
    SELECT target.relname,
           rows.exact_rows,
           COALESCE(stats.n_live_tup, 0)::bigint AS estimated_live_rows,
           COALESCE(stats.n_dead_tup, 0)::bigint AS estimated_dead_rows,
           COALESCE(stats.seq_scan, 0)::bigint AS seq_scan,
           COALESCE(stats.idx_scan, 0)::bigint AS idx_scan,
           COALESCE(stats.n_tup_ins, 0)::bigint AS tuples_inserted,
           COALESCE(stats.n_tup_upd, 0)::bigint AS tuples_updated,
           COALESCE(stats.n_tup_del, 0)::bigint AS tuples_deleted,
           COALESCE(stats.autovacuum_count, 0)::bigint AS autovacuum_count,
           COALESCE(stats.autoanalyze_count, 0)::bigint AS autoanalyze_count,
           stats.last_autovacuum,
           stats.last_autoanalyze,
           pg_relation_size(class.oid)::bigint AS heap_bytes,
           pg_indexes_size(class.oid)::bigint AS index_bytes,
           pg_total_relation_size(class.oid)::bigint AS total_bytes
      FROM target
      JOIN pg_namespace namespace ON namespace.nspname = current_schema()
      JOIN pg_class class ON class.relnamespace = namespace.oid
                         AND class.relname = target.relname
      JOIN row_counts rows ON rows.relname = target.relname
      LEFT JOIN pg_stat_user_tables stats
             ON stats.schemaname = current_schema()
            AND stats.relname = target.relname
),
index_metrics AS (
    SELECT indexes.relname AS table_name,
           indexes.indexrelname AS index_name,
           pg_relation_size(indexes.indexrelid)::bigint AS bytes,
           COALESCE(indexes.idx_scan, 0)::bigint AS scans,
           COALESCE(indexes.idx_tup_read, 0)::bigint AS tuples_read,
           COALESCE(indexes.idx_tup_fetch, 0)::bigint AS tuples_fetched
      FROM pg_stat_user_indexes indexes
     WHERE indexes.schemaname = current_schema()
       AND indexes.relname IN (SELECT relname FROM target)
),
database_metrics AS (
    SELECT xact_commit::bigint, xact_rollback::bigint,
           blks_read::bigint, blks_hit::bigint,
           tup_returned::bigint, tup_fetched::bigint,
           tup_inserted::bigint, tup_updated::bigint, tup_deleted::bigint,
           temp_files::bigint, temp_bytes::bigint, deadlocks::bigint,
           blk_read_time::double precision, blk_write_time::double precision,
           stats_reset
      FROM pg_stat_database WHERE datname = current_database()
)
SELECT json_build_object(
    'captured_at', clock_timestamp(),
    'database', current_database(),
    'server_version', current_setting('server_version'),
    'wal_lsn', pg_current_wal_lsn()::text,
    'wal_bytes_from_origin', pg_wal_lsn_diff(pg_current_wal_lsn(), '0/0')::numeric,
    'database_size_bytes', pg_database_size(current_database())::bigint,
    'active_database_connections', (
        SELECT COUNT(*)::bigint FROM pg_stat_activity WHERE datname = current_database()
    ),
    'tables', COALESCE((SELECT json_agg(table_metrics ORDER BY relname) FROM table_metrics), '[]'),
    'indexes', COALESCE((SELECT json_agg(index_metrics ORDER BY table_name, index_name) FROM index_metrics), '[]'),
    'database_activity', (SELECT row_to_json(database_metrics) FROM database_metrics),
    'journal_accounting', (
        SELECT json_build_object(
            'schema_version', schema_version,
            'retained_events', retained_events,
            'retained_bytes', retained_bytes,
            'actual_rows', (SELECT COUNT(*)::bigint FROM {events}),
            'payload_bytes', (SELECT COALESCE(SUM(payload_bytes), 0)::bigint FROM {events}),
            'accounted_bytes', (SELECT COALESCE(SUM(accounted_bytes), 0)::bigint FROM {events})
        ) FROM {usage} WHERE singleton = TRUE
    ),
    'human_input_rows', (
        SELECT json_build_object(
            'total', COUNT(*)::bigint,
            'pending', COUNT(*) FILTER (WHERE state = 'pending')::bigint,
            'answered', COUNT(*) FILTER (WHERE state = 'answered')::bigint
        ) FROM {human}
    )
)::text;
"""


def pg_stat_statements_sql(prefix: str) -> str:
    validate_prefix(prefix)
    escaped = prefix.replace("_", "\\_")
    return f"""
SELECT COALESCE(json_agg(observation ORDER BY operation, queryid), '[]')::text
FROM (
    SELECT queryid::text,
           CASE
             WHEN query ILIKE '%{escaped}human\\_inputs%' ESCAPE '\\' THEN 'human_input'
             WHEN query ILIKE '%{escaped}run\\_events%' ESCAPE '\\'
               OR query ILIKE '%{escaped}run\\_event\\_state%' ESCAPE '\\' THEN 'run_event'
             WHEN query ILIKE '%{escaped}runs%' ESCAPE '\\' THEN 'runs'
             ELSE 'other'
           END AS operation,
           calls::bigint,
           rows::bigint,
           total_exec_time::double precision,
           shared_blks_hit::bigint,
           shared_blks_read::bigint,
           temp_blks_written::bigint
      FROM pg_stat_statements
     WHERE query ILIKE '%{escaped}%' ESCAPE '\\'
       AND query NOT ILIKE '%pg_stat_statements%'
       AND query NOT ILIKE '%pg_stat_database%'
) observation;
"""


def collect_pg_stat_statements(client: PostgresClient, prefix: str) -> dict[str, Any]:
    try:
        available = client.execute(
            "SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = "
            "'pg_stat_statements')::text;"
        )
        if available not in {"t", "true", "1"}:
            return {"available": False, "reason": "extension_not_installed", "statements": []}
        return {
            "available": True,
            "reason": None,
            "statements": client.json(pg_stat_statements_sql(prefix)) or [],
        }
    except Exception as error:
        return {
            "available": False,
            "reason": sanitize_error(
                error,
                (
                    client.database_url,
                    urllib.parse.unquote(client.parsed.password or ""),
                ),
            ),
            "statements": [],
        }


def numeric_delta(before: Any, after: Any) -> int | float | None:
    if isinstance(before, (int, float)) and isinstance(after, (int, float)):
        return after - before
    return None


def keyed_delta(
    before: list[dict[str, Any]],
    after: list[dict[str, Any]],
    keys: tuple[str, ...],
    fields: tuple[str, ...],
) -> list[dict[str, Any]]:
    def identity(row: dict[str, Any]) -> tuple[Any, ...]:
        return tuple(row.get(key) for key in keys)

    old = {identity(row): row for row in before}
    output: list[dict[str, Any]] = []
    for row in after:
        item = {key: row.get(key) for key in keys}
        previous = old.get(identity(row), {})
        for field in fields:
            item[field] = numeric_delta(previous.get(field, 0), row.get(field))
        output.append(item)
    return output


def postgres_delta(before: dict[str, Any], after: dict[str, Any]) -> dict[str, Any]:
    table_fields = (
        "exact_rows",
        "estimated_live_rows",
        "estimated_dead_rows",
        "seq_scan",
        "idx_scan",
        "tuples_inserted",
        "tuples_updated",
        "tuples_deleted",
        "autovacuum_count",
        "autoanalyze_count",
        "heap_bytes",
        "index_bytes",
        "total_bytes",
    )
    index_fields = ("bytes", "scans", "tuples_read", "tuples_fetched")
    activity_before = before.get("database_activity") or {}
    activity_after = after.get("database_activity") or {}
    activity_delta = {
        key: numeric_delta(activity_before.get(key), activity_after.get(key))
        for key in activity_after
        if key != "stats_reset"
    }
    accounting_before = before.get("journal_accounting") or {}
    accounting_after = after.get("journal_accounting") or {}
    human_before = before.get("human_input_rows") or {}
    human_after = after.get("human_input_rows") or {}
    return {
        "wal_bytes": numeric_delta(
            before.get("wal_bytes_from_origin"), after.get("wal_bytes_from_origin")
        ),
        "database_size_bytes": numeric_delta(
            before.get("database_size_bytes"), after.get("database_size_bytes")
        ),
        "tables": keyed_delta(
            before.get("tables", []), after.get("tables", []), ("relname",), table_fields
        ),
        "indexes": keyed_delta(
            before.get("indexes", []),
            after.get("indexes", []),
            ("table_name", "index_name"),
            index_fields,
        ),
        "journal_accounting": {
            key: numeric_delta(accounting_before.get(key), accounting_after.get(key))
            for key in (
                "retained_events",
                "retained_bytes",
                "actual_rows",
                "payload_bytes",
                "accounted_bytes",
            )
        },
        "human_input_rows": {
            key: numeric_delta(human_before.get(key), human_after.get(key))
            for key in ("total", "pending", "answered")
        },
        "database_activity": activity_delta,
        "stats_reset_changed": activity_before.get("stats_reset")
        != activity_after.get("stats_reset"),
        "attribution": {
            "table_and_index_metrics": "measured for the isolated IronCrew prefix",
            "wal_and_database_activity": (
                "measured database-wide; concurrent workloads can contribute to these deltas"
            ),
            "row_counts": "exact COUNT(*) at each boundary",
            "live_dead_and_autovacuum": "PostgreSQL statistics collector estimates/counters",
        },
    }


def pg_stat_statements_delta(
    before: dict[str, Any], after: dict[str, Any]
) -> dict[str, Any]:
    if not before.get("available") or not after.get("available"):
        return {
            "available": False,
            "reason": after.get("reason") or before.get("reason"),
            "statements": [],
        }
    return {
        "available": True,
        "reason": None,
        "statements": keyed_delta(
            before.get("statements", []),
            after.get("statements", []),
            ("operation", "queryid"),
            (
                "calls",
                "rows",
                "total_exec_time",
                "shared_blks_hit",
                "shared_blks_read",
                "temp_blks_written",
            ),
        ),
        "attribution": (
            "measured statement counters filtered by table prefix; unavailable without "
            "pg_stat_statements and sufficient permissions"
        ),
    }


def cleanup_sql(prefix: str) -> str:
    validate_prefix(prefix)
    drops = "\n".join(
        f"DROP TABLE IF EXISTS {prefix}{suffix};" for suffix in reversed(TABLE_SUFFIXES)
    )
    return (
        f"{drops}\n"
        f"DROP FUNCTION IF EXISTS {prefix}idempotency_acct_fn();\n"
        f"DROP FUNCTION IF EXISTS {prefix}run_events_acct_fn();\n"
    )


def file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def default_binary() -> Path:
    release = ROOT / "target" / "release" / "ironcrew"
    debug = ROOT / "target" / "debug" / "ironcrew"
    if release.is_file():
        return release
    return debug


def docker_pid(container: str) -> int | None:
    if platform.system() != "Linux":
        # Docker Desktop's container PID belongs to its Linux VM. Treating the
        # same number as a macOS host PID can silently sample an unrelated
        # process and produce false per-replica RSS evidence.
        return None
    result = subprocess.run(
        ["docker", "inspect", "--format", "{{.State.Pid}}", container],
        check=True,
        capture_output=True,
        text=True,
        timeout=10,
    )
    pid = int(result.stdout.strip())
    if pid <= 0:
        raise RuntimeError(f"container {container!r} is not running")
    return pid


class ReplicaLauncher:
    def __init__(self, report_dir: Path) -> None:
        self.report_dir = report_dir
        self.processes: dict[str, subprocess.Popen[bytes]] = {}
        self.logs: dict[str, Any] = {}

    def start(
        self,
        name: str,
        binary: Path,
        host: str,
        port: int,
        environment: dict[str, str],
    ) -> subprocess.Popen[bytes]:
        log_path = self.report_dir / f"replica-{name}.log"
        log_handle = log_path.open("wb")
        command = [
            str(binary),
            "serve",
            "--host",
            host,
            "--port",
            str(port),
            "--flows-dir",
            str(FLOW_ROOT),
        ]
        process = subprocess.Popen(
            command,
            cwd=ROOT,
            env=environment,
            stdout=log_handle,
            stderr=subprocess.STDOUT,
            start_new_session=os.name == "posix",
        )
        self.processes[name] = process
        self.logs[name] = log_handle
        return process

    def stop_all(self, grace_seconds: float = 10.0) -> dict[str, Any]:
        outcome: dict[str, Any] = {}
        for name, process in self.processes.items():
            if process.poll() is None:
                try:
                    if os.name == "posix":
                        os.killpg(process.pid, signal.SIGTERM)
                    else:
                        process.terminate()
                except ProcessLookupError:
                    pass
        deadline = time.monotonic() + grace_seconds
        for name, process in self.processes.items():
            remaining = max(0.0, deadline - time.monotonic())
            try:
                process.wait(timeout=remaining)
                forced = False
            except subprocess.TimeoutExpired:
                if os.name == "posix":
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                else:
                    process.kill()
                process.wait(timeout=5)
                forced = True
            outcome[name] = {"exit_code": process.returncode, "forced_kill": forced}
        for handle in self.logs.values():
            handle.close()
        return outcome


def wait_ready(
    client: HttpClient,
    base_url: str,
    process: subprocess.Popen[bytes] | None,
    timeout: float,
) -> None:
    deadline = time.monotonic() + timeout
    last_status: int | None = None
    while time.monotonic() < deadline:
        if process is not None and process.poll() is not None:
            raise RuntimeError(f"replica exited during startup with code {process.returncode}")
        try:
            response = client.request("startup_ready", "GET", f"{base_url}/health/ready")
            last_status = response.status
            if response.status == 200:
                return
        except Exception:
            pass
        time.sleep(0.1)
    raise RuntimeError(f"replica readiness timed out; last HTTP status={last_status}")


def child_environment(
    args: argparse.Namespace,
    instance_id: str,
    token: str,
    prefix: str,
    keyring_json: str,
) -> dict[str, str]:
    environment = os.environ.copy()
    for name in ("PORT", "IRONCREW_PORT", "IRONCREW_HOST"):
        environment.pop(name, None)
    environment.update(
        {
            "IRONCREW_STORE": "postgres",
            "DATABASE_URL": args.database_url,
            "IRONCREW_PG_TABLE_PREFIX": prefix,
            "IRONCREW_INSTANCE_ID": instance_id,
            "IRONCREW_API_TOKEN": token,
            "IRONCREW_REQUIRE_IDEMPOTENCY_KEY": "true",
            "IRONCREW_HITL_ENCRYPTION_KEYS": keyring_json,
            "IRONCREW_HITL_ACTIVE_KEY_ID": "soak-v1",
            "IRONCREW_DB_POOL_SIZE": str(args.db_pool_size),
            "IRONCREW_MAX_ACTIVE_RUNS": str(args.max_active_runs),
            "IRONCREW_MAX_SSE_CONNECTIONS": str(max(4, args.max_active_runs * 2)),
            "IRONCREW_HITL_POLL_INTERVAL_MS": str(args.hitl_poll_ms),
            "IRONCREW_HITL_PG_MAX_CONCURRENT_READS": str(args.hitl_pg_reads),
            "IRONCREW_EVENT_JOURNAL_POLL_INTERVAL_MS": str(args.journal_poll_ms),
            "IRONCREW_MAX_EVENTS": str(args.max_events),
            "IRONCREW_RUN_LEASE_TTL_SECONDS": "10",
            "IRONCREW_EVENT_JOURNAL_RETENTION_SECS": "600",
            "IRONCREW_LOG": args.log_level,
            # Fail closed even if this fixture is accidentally changed to call
            # crew:run(). Existing values beat repository .env loading.
            "OPENAI_API_KEY": "replica-soak-no-network",
            "OPENAI_BASE_URL": "http://127.0.0.1:9/v1",
        }
    )
    return environment


class RunAllocator:
    def __init__(self, maximum: int, deadline: float) -> None:
        self.maximum = maximum
        self.deadline = deadline
        self.next_index = 0
        self.lock = threading.Lock()

    def take(self) -> int | None:
        with self.lock:
            if self.next_index >= self.maximum or time.monotonic() >= self.deadline:
                return None
            index = self.next_index
            self.next_index += 1
            return index


def execute_run(
    index: int,
    bases: tuple[str, str],
    client: HttpClient,
    args: argparse.Namespace,
) -> dict[str, Any]:
    owner_index = index % 2
    peer_index = 1 - owner_index
    owner = bases[owner_index]
    peer = bases[peer_index]
    started_at = time.perf_counter()
    result: dict[str, Any] = {
        "index": index,
        "owner_replica": "a" if owner_index == 0 else "b",
        "peer_replica": "a" if peer_index == 0 else "b",
        "success": False,
        "run_id": None,
    }
    try:
        idempotency_key = f"replica-soak-{index:08d}-{uuid.uuid4().hex}"
        started = client.request(
            "run_start",
            "POST",
            f"{owner}/flows/soak/run",
            {},
            {"Idempotency-Key": idempotency_key},
        )
        if started.status != 200:
            raise RuntimeError(f"run start returned HTTP {started.status}")
        started_body = started.json()
        run_id = started_body.get("run_id")
        if not isinstance(run_id, str) or not run_id:
            raise RuntimeError("run start omitted run_id")
        result["run_id"] = run_id

        initial_sse = client.sse_until(
            "sse_initial",
            f"{peer}/flows/soak/events/{run_id}",
            "human_input_requested",
            max_bytes=args.max_sse_bytes,
        )
        cursor = initial_sse.get("id")
        if not isinstance(cursor, str) or not cursor.startswith(f"{run_id}:"):
            raise RuntimeError("initial SSE omitted a run-scoped cursor")
        pending_started = time.perf_counter()
        question: dict[str, Any] | None = None
        question_deadline = time.monotonic() + args.request_timeout
        while time.monotonic() < question_deadline:
            questions = client.request(
                "questions_poll", "GET", f"{peer}/flows/soak/questions/{run_id}"
            )
            if questions.status == 200:
                payload = questions.json()
                rows = payload.get("questions") if isinstance(payload, dict) else None
                if isinstance(rows, list) and rows:
                    question = rows[0]
                    break
            elif questions.status not in {404, 409, 503}:
                raise RuntimeError(f"question poll returned HTTP {questions.status}")
            time.sleep(args.client_poll_interval)
        if question is None:
            raise RuntimeError("durable question did not become visible")
        question_id = question.get("question_id")
        if not isinstance(question_id, str):
            raise RuntimeError("question response omitted question_id")

        answered = client.request(
            "answer_peer",
            "POST",
            f"{peer}/flows/soak/answer/{run_id}",
            {"question_id": question_id, "answer": "continue"},
        )
        if answered.status not in {200, 202}:
            raise RuntimeError(f"answer returned HTTP {answered.status}")
        answer_ack_at = time.perf_counter()
        question_to_answer_ack_ms = (answer_ack_at - pending_started) * 1000

        terminal_sse = client.sse_until(
            "sse_reconnect",
            f"{peer}/flows/soak/events/{run_id}",
            "run_complete",
            last_event_id=cursor,
            max_bytes=args.max_sse_bytes,
        )
        terminal_data = sse_event_payload(terminal_sse)
        if not isinstance(terminal_data, dict) or terminal_data.get("status") != "success":
            raise RuntimeError("terminal SSE did not report success")
        terminal_observed_at = time.perf_counter()
        pending_ms = (terminal_observed_at - pending_started) * 1000
        answer_to_terminal_ms = (terminal_observed_at - answer_ack_at) * 1000

        run_record = client.request(
            "run_read_peer", "GET", f"{peer}/flows/soak/runs/{run_id}"
        )
        if run_record.status != 200:
            raise RuntimeError(f"peer run read returned HTTP {run_record.status}")
        record_body = run_record.json()
        if not isinstance(record_body, dict) or record_body.get("status") != "Success":
            raise RuntimeError("peer run record did not report success")

        result.update(
            {
                "success": True,
                "pending_ms": round(pending_ms, 3),
                "question_to_answer_ack_ms": round(question_to_answer_ack_ms, 3),
                "answer_to_terminal_ms": round(answer_to_terminal_ms, 3),
                "initial_sse_bytes": initial_sse["bytes"],
                "reconnect_sse_bytes": terminal_sse["bytes"],
                "duration_ms": round((time.perf_counter() - started_at) * 1000, 3),
            }
        )
    except Exception as error:
        result.update(
            {
                "error_kind": type(error).__name__,
                "error": sanitize_error(error),
                "duration_ms": round((time.perf_counter() - started_at) * 1000, 3),
            }
        )
    return result


def run_workload(
    bases: tuple[str, str],
    client: HttpClient,
    args: argparse.Namespace,
) -> list[dict[str, Any]]:
    allocator = RunAllocator(args.runs, time.monotonic() + args.duration_seconds)
    results: list[dict[str, Any]] = []
    results_lock = threading.Lock()

    def worker() -> None:
        while (index := allocator.take()) is not None:
            result = execute_run(index, bases, client, args)
            with results_lock:
                results.append(result)

    with concurrent.futures.ThreadPoolExecutor(
        max_workers=args.concurrency, thread_name_prefix="soak-run"
    ) as executor:
        futures = [executor.submit(worker) for _ in range(args.concurrency)]
        for future in futures:
            future.result()
    return sorted(results, key=lambda item: item["index"])


class HealthProbe:
    def __init__(
        self, bases: tuple[str, str], client: HttpClient, interval: float
    ) -> None:
        self.bases = bases
        self.client = client
        self.interval = interval
        self.stop_event = threading.Event()
        self.thread: threading.Thread | None = None

    def start(self) -> None:
        if self.interval <= 0:
            return
        self.thread = threading.Thread(target=self._run, name="health-probe", daemon=True)
        self.thread.start()

    def stop(self) -> bool:
        self.stop_event.set()
        if self.thread:
            self.thread.join(
                timeout=max(2.0, self.client.timeout + self.interval + 1.0)
            )
            return not self.thread.is_alive()
        return True

    def _run(self) -> None:
        index = 0
        while not self.stop_event.is_set():
            base = self.bases[index % 2]
            for operation, path in (
                ("health_liveness_probe", "/health"),
                ("health_readiness_probe", "/health/ready"),
            ):
                try:
                    self.client.request(operation, "GET", f"{base}{path}")
                except Exception:
                    pass
            index += 1
            self.stop_event.wait(self.interval)


def bounded_int(name: str, value: int, minimum: int, maximum: int) -> int:
    if not minimum <= value <= maximum:
        raise ValueError(f"{name} must be between {minimum} and {maximum}")
    return value


def bounded_float(name: str, value: float, minimum: float, maximum: float) -> float:
    if not minimum <= value <= maximum:
        raise ValueError(f"{name} must be between {minimum} and {maximum}")
    return value


def validate_base_url(name: str, value: str) -> str:
    parsed = urllib.parse.urlsplit(value)
    if parsed.scheme not in {"http", "https"} or not parsed.hostname:
        raise ValueError(f"{name} must be an absolute http(s) URL")
    if parsed.username or parsed.password or parsed.query or parsed.fragment:
        raise ValueError(f"{name} must not contain credentials, query, or fragment")
    return value.rstrip("/")


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Bounded two-replica IronCrew/PostgreSQL soak evaluator"
    )
    parser.add_argument("--mode", choices=("launch", "target"), default="launch")
    parser.add_argument("--database-url", default=os.environ.get("DATABASE_URL"))
    parser.add_argument("--postgres-container", help="run psql inside this local Docker container")
    parser.add_argument("--psql", dest="psql_command", help="path to psql (auto-detected by default)")
    parser.add_argument("--table-prefix")
    parser.add_argument("--cleanup-database", action="store_true")
    parser.add_argument("--keep-database", action="store_true")
    parser.add_argument("--binary", type=Path, default=default_binary())
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port-a", type=int, default=3311)
    parser.add_argument("--port-b", type=int, default=3312)
    parser.add_argument("--base-a")
    parser.add_argument("--base-b")
    parser.add_argument("--pid-a", type=int)
    parser.add_argument("--pid-b", type=int)
    parser.add_argument("--docker-container-a")
    parser.add_argument("--docker-container-b")
    parser.add_argument(
        "--api-token-env",
        default="IRONCREW_API_TOKEN",
        help="target-mode bearer token environment variable name",
    )
    parser.add_argument("--runs", type=int, default=12)
    parser.add_argument("--duration-seconds", type=float, default=60.0)
    parser.add_argument("--concurrency", type=int, default=2)
    parser.add_argument("--request-timeout", type=float, default=20.0)
    parser.add_argument("--startup-timeout", type=float, default=30.0)
    parser.add_argument("--client-poll-interval", type=float, default=0.10)
    parser.add_argument("--health-interval", type=float, default=0.50)
    parser.add_argument("--resource-sample-interval", type=float, default=0.25)
    parser.add_argument("--max-sse-bytes", type=int, default=1024 * 1024)
    parser.add_argument("--max-run-details", type=int, default=100)
    parser.add_argument("--db-pool-size", type=int, default=2)
    parser.add_argument("--max-active-runs", type=int, default=2)
    parser.add_argument("--hitl-poll-ms", type=int, default=1000)
    parser.add_argument("--hitl-pg-reads", type=int, default=2)
    parser.add_argument("--journal-poll-ms", type=int, default=500)
    parser.add_argument("--max-events", type=int, default=200)
    parser.add_argument(
        "--memory-comparator-mib",
        type=int,
        default=1024,
        help="report-only Railway/OpenShift comparator; never enforced locally",
    )
    parser.add_argument("--log-level", default="warn")
    parser.add_argument("--report", type=Path)
    args = parser.parse_args(argv)

    if not args.database_url:
        parser.error("--database-url or DATABASE_URL is required")
    try:
        bounded_int("runs", args.runs, 1, 10_000)
        bounded_float("duration-seconds", args.duration_seconds, 1.0, 3600.0)
        bounded_int("concurrency", args.concurrency, 1, 64)
        bounded_float("request-timeout", args.request_timeout, 1.0, 120.0)
        bounded_float("startup-timeout", args.startup_timeout, 1.0, 300.0)
        bounded_float("client-poll-interval", args.client_poll_interval, 0.01, 5.0)
        bounded_float("health-interval", args.health_interval, 0.0, 60.0)
        bounded_float("resource-sample-interval", args.resource_sample_interval, 0.05, 10.0)
        bounded_int("max-sse-bytes", args.max_sse_bytes, 1024, 16 * 1024 * 1024)
        bounded_int("max-run-details", args.max_run_details, 0, MAX_RUN_DETAILS_HARD)
        bounded_int("db-pool-size", args.db_pool_size, 1, 100)
        bounded_int("max-active-runs", args.max_active_runs, 1, 1024)
        bounded_int("hitl-poll-ms", args.hitl_poll_ms, 50, 5000)
        bounded_int("hitl-pg-reads", args.hitl_pg_reads, 1, 64)
        bounded_int("journal-poll-ms", args.journal_poll_ms, 100, 5000)
        bounded_int("max-events", args.max_events, 1, 10_000)
        bounded_int("memory-comparator-mib", args.memory_comparator_mib, 64, 1024 * 1024)
        bounded_int("port-a", args.port_a, 1, 65535)
        bounded_int("port-b", args.port_b, 1, 65535)
    except ValueError as error:
        parser.error(str(error))
    if args.port_a == args.port_b:
        parser.error("--port-a and --port-b must differ")
    if args.cleanup_database and args.keep_database:
        parser.error("--cleanup-database and --keep-database are mutually exclusive")
    if args.mode == "target" and (not args.base_a or not args.base_b):
        parser.error("target mode requires --base-a and --base-b")
    if args.mode == "target" and not args.table_prefix:
        parser.error("target mode requires --table-prefix for scoped PostgreSQL metrics")
    if args.mode == "launch" and not args.binary.is_file():
        parser.error(f"IronCrew binary not found: {args.binary}")
    if args.table_prefix:
        try:
            validate_prefix(args.table_prefix)
        except ValueError as error:
            parser.error(str(error))
    return args


def git_revision() -> dict[str, Any]:
    try:
        revision = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
            timeout=5,
        ).stdout.strip()
        dirty = bool(
            subprocess.run(
                ["git", "status", "--porcelain"],
                cwd=ROOT,
                check=True,
                capture_output=True,
                text=True,
                timeout=5,
            ).stdout.strip()
        )
        return {"revision": revision, "dirty": dirty}
    except (OSError, subprocess.SubprocessError):
        return {"revision": None, "dirty": None}


def choose_details(results: list[dict[str, Any]], maximum: int) -> list[dict[str, Any]]:
    if maximum == 0:
        return []
    failures = [result for result in results if not result.get("success")]
    successes = [result for result in results if result.get("success")]
    return (failures + successes)[:maximum]


def aggregate_workload(
    results: list[dict[str, Any]],
    metrics: OperationMetrics,
    args: argparse.Namespace,
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
        "result_details": choose_details(results, args.max_run_details),
        "result_details_truncated": len(results) > args.max_run_details,
    }


def build_pass_criteria(
    report: dict[str, Any],
    metrics: OperationMetrics,
    args: argparse.Namespace,
    launcher: ReplicaLauncher,
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
        replica_states = {}
        replicas_alive = True
    comparator = args.memory_comparator_mib * 1024 * 1024
    available_peaks = {
        name: values.get("sampled_peak_rss_bytes")
        for name, values in resources.items()
        if isinstance(values.get("sampled_peak_rss_bytes"), int)
    }
    rss_ok = all(peak < comparator for peak in available_peaks.values())
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
            "passed": replicas_alive,
            "applicable": args.mode == "launch",
            "states": replica_states,
        },
        "postgres_deadlocks": {"passed": deadlocks == 0, "delta": deadlocks},
        "postgres_stats_reset_stable": {"passed": stats_stable},
        "host_process_rss_comparator": {
            "passed": rss_ok,
            "applicable": bool(available_peaks),
            "sampled_peaks_bytes": available_peaks,
            "comparator_bytes": comparator,
            "comparator_enforced_by_runner": False,
            "scope": "host_process_rss",
        },
    }
    criteria["overall_passed"] = all(
        value.get("passed") is True
        for value in criteria.values()
        if isinstance(value, dict)
    )
    return criteria


def write_report(path: Path, report: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    temporary.replace(path)


def execute(args: argparse.Namespace) -> tuple[dict[str, Any], int]:
    started_at = utc_now()
    started_monotonic = time.monotonic()
    generated_prefix = args.table_prefix is None
    prefix = validate_prefix(args.table_prefix or f"soak_{secrets.token_hex(4)}_")
    timestamp = datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ")
    report_path = args.report or REPORT_ROOT / f"replica-soak-{timestamp}.json"
    report_path = report_path.resolve()
    report_dir = report_path.parent
    report_dir.mkdir(parents=True, exist_ok=True)
    token = (
        secrets.token_urlsafe(32)
        if args.mode == "launch"
        else os.environ.get(args.api_token_env)
    )
    metrics = OperationMetrics()
    client = HttpClient(token, args.request_timeout, metrics)
    pg_client = PostgresClient(
        args.database_url, args.psql_command, args.postgres_container
    )
    report_secrets = (
        args.database_url,
        urllib.parse.unquote(pg_client.parsed.password or ""),
        token or "",
    )
    launcher = ReplicaLauncher(report_dir)
    sampler: ResourceSampler | None = None
    health: HealthProbe | None = None
    cleanup_performed = False
    shutdown: dict[str, Any] = {}
    report: dict[str, Any] = {
        "schema_version": SCHEMA_VERSION,
        "status": "running",
        "started_at": started_at,
        "mode": args.mode,
        "platform": {
            "system": platform.system(),
            "release": platform.release(),
            "machine": platform.machine(),
            "python": platform.python_version(),
        },
        "source": {**git_revision()},
        "configuration": {
            "database": safe_database_label(args.database_url),
            "table_prefix": prefix,
            "runs": args.runs,
            "duration_seconds": args.duration_seconds,
            "concurrency": args.concurrency,
            "request_timeout_seconds": args.request_timeout,
            "resource_sample_interval_seconds": args.resource_sample_interval,
            "db_pool_size_per_replica": args.db_pool_size,
            "max_active_runs_per_replica": args.max_active_runs,
            "hitl_poll_interval_ms": args.hitl_poll_ms,
            "hitl_pg_max_concurrent_reads": args.hitl_pg_reads,
            "journal_poll_interval_ms": args.journal_poll_ms,
            "max_events_per_run": args.max_events,
            "memory_comparator_bytes_per_replica": args.memory_comparator_mib * 1024 * 1024,
            "memory_comparator_enforced_by_runner": False,
            "authentication_configured": bool(token),
            "idempotency_key_required": True if args.mode == "launch" else None,
            "idempotency_keys_sent": True,
            "llm_calls_planned": 0,
        },
        "notes": [
            "The flow never calls crew:run(); provider credentials and base URL are fail-closed fixtures.",
            "Host-process RSS and pod/container cgroup memory are separate scopes.",
            "The 1 GiB default is a report comparator, not a locally enforced Railway/OpenShift limit.",
            "WAL and pg_stat_database deltas are database-wide; use an isolated database for attribution.",
        ],
        "postgres": {},
        "cleanup": {},
    }
    exit_code = 1
    try:
        pg_client.execute("SELECT 1;")
        cleanup_allowed = (generated_prefix and args.mode == "launch") or args.cleanup_database
        if cleanup_allowed:
            pg_client.execute(cleanup_sql(prefix))

        if args.mode == "launch":
            binary = args.binary.resolve()
            report["source"].update(
                {
                    "binary": str(binary),
                    "binary_sha256": file_sha256(binary),
                    "binary_profile": "release"
                    if binary.parent.name == "release"
                    else "non_release",
                    "release_binary_preferred": True,
                }
            )
            key = base64.b64encode(secrets.token_bytes(32)).decode()
            keyring_json = json.dumps({"soak-v1": key}, separators=(",", ":"))
            base_a = validate_base_url("base-a", f"http://{args.host}:{args.port_a}")
            base_b = validate_base_url("base-b", f"http://{args.host}:{args.port_b}")
            process_a = launcher.start(
                "a",
                binary,
                args.host,
                args.port_a,
                child_environment(args, "soak-replica-a", token or "", prefix, keyring_json),
            )
            wait_ready(client, base_a, process_a, args.startup_timeout)
            process_b = launcher.start(
                "b",
                binary,
                args.host,
                args.port_b,
                child_environment(args, "soak-replica-b", token or "", prefix, keyring_json),
            )
            wait_ready(client, base_b, process_b, args.startup_timeout)
            pids = {"a": process_a.pid, "b": process_b.pid}
        else:
            base_a = validate_base_url("base-a", args.base_a)
            base_b = validate_base_url("base-b", args.base_b)
            pid_a = args.pid_a or (
                docker_pid(args.docker_container_a) if args.docker_container_a else None
            )
            pid_b = args.pid_b or (
                docker_pid(args.docker_container_b) if args.docker_container_b else None
            )
            pids = {"a": pid_a, "b": pid_b}
            wait_ready(client, base_a, None, args.startup_timeout)
            wait_ready(client, base_b, None, args.startup_timeout)

        bases = (base_a, base_b)
        report["configuration"]["replicas"] = {
            "a": {"base_url": base_a, "pid": pids["a"]},
            "b": {"base_url": base_b, "pid": pids["b"]},
        }
        sampler = ResourceSampler(
            pids,
            args.resource_sample_interval,
            args.memory_comparator_mib * 1024 * 1024,
        )
        sampler.start()
        before = pg_client.json(postgres_snapshot_sql(prefix))
        statements_before = collect_pg_stat_statements(pg_client, prefix)
        report["postgres"]["before"] = before
        health = HealthProbe(bases, client, args.health_interval)
        health.start()
        workload_started = time.monotonic()
        results = run_workload(bases, client, args)
        workload_seconds = time.monotonic() - workload_started
        if not health.stop():
            raise RuntimeError("periodic health probe did not stop within its bounded timeout")
        health = None
        after = pg_client.json(postgres_snapshot_sql(prefix))
        statements_after = collect_pg_stat_statements(pg_client, prefix)
        report["postgres"].update(
            {
                "after": after,
                "delta": postgres_delta(before, after),
                "pg_stat_statements": pg_stat_statements_delta(
                    statements_before, statements_after
                ),
            }
        )
        sampler.stop()
        report["resources"] = sampler.report()
        sampler = None
        report["workload"] = aggregate_workload(results, metrics, args)
        report["workload"]["elapsed_seconds"] = round(workload_seconds, 3)
        report["pass_criteria"] = build_pass_criteria(report, metrics, args, launcher)
        report["status"] = (
            "passed" if report["pass_criteria"]["overall_passed"] else "failed"
        )
        exit_code = 0 if report["status"] == "passed" else 1
    except Exception as error:
        report["status"] = "failed"
        report["error"] = {
            "kind": type(error).__name__,
            "message": sanitize_error(error, report_secrets),
        }
    finally:
        if health:
            health.stop()
        if sampler:
            sampler.stop()
            report["resources"] = sampler.report()
        if launcher.processes:
            shutdown = launcher.stop_all()
        cleanup_requested = (
            not args.keep_database
            and ((generated_prefix and args.mode == "launch") or args.cleanup_database)
        )
        cleanup_error = None
        if cleanup_requested:
            try:
                pg_client.execute(cleanup_sql(prefix))
                cleanup_performed = True
            except Exception as error:
                cleanup_error = sanitize_error(error, report_secrets)
                exit_code = 1
                report["status"] = "failed"
        report["cleanup"] = {
            "replica_shutdown": shutdown,
            "database_cleanup_requested": cleanup_requested,
            "database_cleanup_performed": cleanup_performed,
            "database_cleanup_error": cleanup_error,
            "scope": f"exact validated prefix {prefix}",
        }
        report["finished_at"] = utc_now()
        report["elapsed_seconds"] = round(time.monotonic() - started_monotonic, 3)
        write_report(report_path, report)
        print(str(report_path))
    return report, exit_code


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    _, exit_code = execute(args)
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
