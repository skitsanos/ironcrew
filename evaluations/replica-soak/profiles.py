#!/usr/bin/env python3
"""Run separate bounded mock profiles for the IC-018 evidence matrix."""

from __future__ import annotations

import argparse
import json
import os
import platform
import secrets
import tempfile
import urllib.parse
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

import soak
from profile_contract import profile_metadata
from profile_provider import ProviderFixture
from profile_receipt import capture_source, finalize_report
from profile_runtime import ProfileLauncher, child_environment, free_loopback_port, scan_logs
from profile_workloads import run_profile_suite


SCHEMA_VERSION = "ironcrew.replica-profiles.v1"
HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
FLOW_ROOT = HERE / "flows"
REPORT_ROOT = HERE / "reports"


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--database-url", default=os.environ.get("DATABASE_URL"))
    parser.add_argument("--postgres-container")
    parser.add_argument("--psql", dest="psql_command")
    parser.add_argument("--binary", type=Path, default=soak.default_binary())
    parser.add_argument("--table-prefix")
    parser.add_argument("--request-timeout", type=float, default=30.0)
    parser.add_argument("--poll-interval", type=float, default=0.1)
    parser.add_argument("--startup-timeout", type=float, default=30.0)
    parser.add_argument("--report", type=Path)
    args = parser.parse_args(argv)
    if not args.database_url:
        parser.error("--database-url or DATABASE_URL is required")
    if not args.binary.is_file():
        parser.error(f"IronCrew binary not found: {args.binary}")
    try:
        soak.bounded_float("request-timeout", args.request_timeout, 1.0, 120.0)
        soak.bounded_float("poll-interval", args.poll_interval, 0.01, 5.0)
        soak.bounded_float("startup-timeout", args.startup_timeout, 1.0, 300.0)
        if args.table_prefix:
            soak.validate_prefix(args.table_prefix)
    except ValueError as error:
        parser.error(str(error))
    return args


def remaining_prefix_objects(pg: soak.PostgresClient, prefix: str) -> int:
    soak.validate_prefix(prefix)
    result = pg.execute(
        "SELECT ((SELECT COUNT(*) FROM pg_class c "
        "JOIN pg_namespace n ON n.oid = c.relnamespace "
        f"WHERE n.nspname = current_schema() AND left(c.relname, {len(prefix)}) = '{prefix}') + "
        "(SELECT COUNT(*) FROM pg_proc p "
        "JOIN pg_namespace n ON n.oid = p.pronamespace "
        f"WHERE n.nspname = current_schema() AND left(p.proname, {len(prefix)}) = '{prefix}'))::text;"
    )
    return int(result)


def execute(args: argparse.Namespace) -> tuple[dict[str, Any], int]:
    prefix = soak.validate_prefix(args.table_prefix or f"ic018p_{secrets.token_hex(4)}_")
    timestamp = datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ")
    report_path = (args.report or REPORT_ROOT / f"replica-profiles-{timestamp}.json").resolve()
    report_path.parent.mkdir(parents=True, exist_ok=True)
    binary = args.binary.resolve()
    token = secrets.token_urlsafe(32)
    parsed_dsn = urllib.parse.urlsplit(args.database_url)
    dsn_password = urllib.parse.unquote(parsed_dsn.password or "")
    secrets_to_remove = (args.database_url, dsn_password, token)
    source_at_start = capture_source(ROOT, binary)
    report: dict[str, Any] = {
        "schema_version": SCHEMA_VERSION,
        "status": "running",
        "started_at": datetime.now(UTC).isoformat(),
        "evidence_boundary": {
            "execution": "local_direct_two_process",
            "provider": "bounded_loopback_mock",
            "provider_free": False,
            "live_provider": False,
            "railway": False,
            "openshift": False,
            "planned_paid_provider_calls": 0,
            "actual_paid_provider_calls": 0,
            "estimated_paid_provider_cost_usd": 0.0,
        },
        "source": source_at_start,
        "configuration": {
            "database": soak.safe_database_label(args.database_url),
            "table_prefix": prefix,
            "profiles": profile_metadata(),
            "postgres_observer": {
                "kind": "docker_exec" if args.postgres_container else "host_psql",
                "container_id": args.postgres_container,
            },
        },
        "platform": {"system": platform.system(), "machine": platform.machine()},
        "cleanup": {},
    }
    pg: soak.PostgresClient | None = None
    launcher: ProfileLauncher | None = None
    try:
        pg = soak.PostgresClient(
            args.database_url, args.psql_command, args.postgres_container
        )
        pg.execute("SELECT 1;")
        pg.execute(soak.cleanup_sql(prefix))
        with tempfile.TemporaryDirectory(prefix="ironcrew-ic018-profiles-") as directory:
            runtime_root = Path(directory)
            logs = runtime_root / "logs"
            outputs = runtime_root / "outputs"
            logs.mkdir()
            outputs.mkdir()
            launcher = ProfileLauncher(runtime_root, logs, binary, FLOW_ROOT)
            with ProviderFixture() as provider:
                metrics = soak.OperationMetrics()
                client = soak.HttpClient(token, args.request_timeout, metrics)
                ports = (free_loopback_port(), free_loopback_port())
                if ports[0] == ports[1]:
                    raise RuntimeError("loopback port allocation collided")
                bases = (f"http://127.0.0.1:{ports[0]}", f"http://127.0.0.1:{ports[1]}")
                try:
                    for index, name in enumerate(("a", "b")):
                        process = launcher.start(
                            name,
                            ports[index],
                            child_environment(
                                args.database_url,
                                prefix,
                                token,
                                f"ic018-profile-{name}",
                                provider.base_url,
                                outputs,
                            ),
                        )
                        soak.wait_ready(client, bases[index], process, args.startup_timeout)
                    identities = []
                    for name, base in zip(("a", "b"), bases, strict=True):
                        response = client.request(f"capabilities_{name}", "GET", f"{base}/capabilities")
                        body = response.json() if response.status == 200 else {}
                        if body.get("instance_id") != f"ic018-profile-{name}":
                            raise RuntimeError("profile replica identity check failed")
                        identities.append(body["instance_id"])
                    report["topology"] = {
                        "kind": "direct_two_process",
                        "instance_ids": identities,
                        "load_balancer_in_scope": False,
                    }
                    report["profiles"] = run_profile_suite(
                        bases,
                        client,
                        provider.counters.snapshot,
                        timeout_seconds=args.request_timeout,
                        poll_interval_seconds=args.poll_interval,
                        sanitize_error=lambda error: soak.sanitize_error(error, secrets_to_remove),
                    )
                    report["mock_provider"] = {
                        "exposure": "loopback_only",
                        "counts": provider.counters.snapshot(),
                        "request_or_response_content_retained": False,
                    }
                    report["http_metrics"] = metrics.report()
                finally:
                    report["shutdown"] = launcher.stop_all()
                report["runtime_logs"] = scan_logs(logs, secrets_to_remove)
    except Exception as error:
        report["error"] = {
            "kind": type(error).__name__,
            "message": soak.sanitize_error(error, secrets_to_remove),
        }
    finally:
        if launcher and any(process.poll() is None for process in launcher.processes.values()):
            report["shutdown"] = launcher.stop_all()
        cleanup_error = None
        remaining_objects = None
        if pg is None:
            cleanup_performed = False
            cleanup_error = "PostgreSQL observer was unavailable for exact-prefix cleanup"
        else:
            try:
                pg.execute(soak.cleanup_sql(prefix))
                remaining_objects = remaining_prefix_objects(pg, prefix)
                cleanup_performed = remaining_objects == 0
                if not cleanup_performed:
                    raise RuntimeError("exact profile prefix cleanup left database objects")
            except Exception as error:
                cleanup_performed = False
                cleanup_error = soak.sanitize_error(error, secrets_to_remove)
        report["cleanup"] = {
            "scope": f"exact validated prefix {prefix}",
            "performed": cleanup_performed,
            "remaining_objects": remaining_objects,
            "zero_verified": remaining_objects == 0,
            "error": cleanup_error,
        }
        source_after_cleanup = None
        source_capture_error = None
        try:
            source_after_cleanup = capture_source(ROOT, binary)
        except Exception as error:
            source_capture_error = soak.sanitize_error(error, secrets_to_remove)
        report["finished_at"] = datetime.now(UTC).isoformat()
        exit_code = finalize_report(
            report,
            source_at_start,
            source_after_cleanup,
            source_capture_error,
        )
        report_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        print(report_path)
    return report, exit_code


def main(argv: list[str] | None = None) -> int:
    report, exit_code = execute(parse_args(argv))
    assert report["status"] in {"passed", "failed"}
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
