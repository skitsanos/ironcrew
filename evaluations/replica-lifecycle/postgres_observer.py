"""Prefix-scoped PostgreSQL observation and cleanup for the IC-020 gate."""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import urllib.parse
from typing import Any

from reporting import sanitize_failure


PREFIX = re.compile(r"ic020cap_[a-f0-9]{8}_")
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


def validate_prefix(prefix: str) -> str:
    if not PREFIX.fullmatch(prefix):
        raise ValueError("IC-020 table prefix has an invalid shape")
    return prefix


def safe_database_label(database_url: str) -> str:
    parsed = urllib.parse.urlsplit(database_url)
    host = parsed.hostname or "unknown"
    port = f":{parsed.port}" if parsed.port else ""
    database = parsed.path.lstrip("/") or "unknown"
    return f"{parsed.scheme}://{host}{port}/{database}"


class PostgresObserver:
    def __init__(self, database_url: str, container: str | None = None) -> None:
        self.database_url = database_url
        self.parsed = urllib.parse.urlsplit(database_url)
        if self.parsed.scheme not in {"postgres", "postgresql"}:
            raise ValueError("DATABASE_URL must use PostgreSQL")
        self.container = container
        self.psql = shutil.which("psql")
        if not container and not self.psql:
            raise RuntimeError("psql or --postgres-container is required")

    def _command(self) -> tuple[list[str], dict[str, str]]:
        user = urllib.parse.unquote(self.parsed.username or "postgres")
        database = urllib.parse.unquote(self.parsed.path.lstrip("/") or user)
        environment = os.environ.copy()
        if self.container:
            return (
                [
                    "docker",
                    "exec",
                    "-i",
                    self.container,
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
                environment,
            )
        assert self.psql is not None
        environment.update(
            {
                "PGHOST": self.parsed.hostname or "127.0.0.1",
                "PGPORT": str(self.parsed.port or 5432),
                "PGUSER": user,
                "PGDATABASE": database,
                "PGPASSWORD": urllib.parse.unquote(self.parsed.password or ""),
            }
        )
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
        try:
            result = subprocess.run(
                command,
                input=sql,
                text=True,
                capture_output=True,
                check=True,
                timeout=30,
                env=environment,
            )
        except subprocess.CalledProcessError as error:
            detail = (error.stderr or error.stdout or "PostgreSQL command failed").strip()
            password = urllib.parse.unquote(self.parsed.password or "")
            raise RuntimeError(
                sanitize_failure(
                    detail,
                    database_url=self.database_url,
                    secret_canaries=(password,),
                    limit=500,
                )
            ) from error
        return result.stdout.strip()

    def json(self, sql: str) -> Any:
        output = self.execute(sql)
        return json.loads(output) if output else None

    def snapshot(self, prefix: str) -> dict[str, Any]:
        validate_prefix(prefix)
        events = f"{prefix}run_events"
        usage = f"{prefix}run_event_usage"
        runs = f"{prefix}runs"
        return self.json(
            f"""
            SELECT json_build_object(
                'server_version', current_setting('server_version'),
                'connections_excluding_observer', (
                    SELECT COUNT(*)::bigint
                      FROM pg_stat_activity
                     WHERE datname = current_database() AND pid <> pg_backend_pid()
                ),
                'journal', (
                    SELECT json_build_object(
                        'schema_version', schema_version,
                        'retained_events', retained_events,
                        'retained_bytes', retained_bytes,
                        'actual_rows', (SELECT COUNT(*)::bigint FROM {events}),
                        'payload_bytes', (
                            SELECT COALESCE(SUM(payload_bytes), 0)::bigint FROM {events}
                        ),
                        'accounted_bytes', (
                            SELECT COALESCE(SUM(accounted_bytes), 0)::bigint FROM {events}
                        ),
                        'maximum_run_events', (
                            SELECT COALESCE(MAX(event_count), 0)::bigint
                              FROM (SELECT COUNT(*)::bigint event_count
                                      FROM {events} GROUP BY run_id) grouped
                        ),
                        'maximum_run_accounted_bytes', (
                            SELECT COALESCE(MAX(event_bytes), 0)::bigint
                              FROM (SELECT SUM(accounted_bytes)::bigint event_bytes
                                      FROM {events} GROUP BY run_id) grouped
                        )
                    ) FROM {usage} WHERE singleton = TRUE
                ),
                'runs', (
                    SELECT json_build_object(
                        'total', COUNT(*)::bigint,
                        'active', COUNT(*) FILTER (
                            WHERE status IN ('running', 'waiting_for_input')
                        )::bigint,
                        'success', COUNT(*) FILTER (WHERE status = 'success')::bigint,
                        'failed', COUNT(*) FILTER (WHERE status = 'failed')::bigint
                    ) FROM {runs}
                )
            )::text;
            """
        )

    def cleanup(self, prefix: str) -> dict[str, Any]:
        validate_prefix(prefix)
        drops = "\n".join(
            f"DROP TABLE IF EXISTS {prefix}{suffix};" for suffix in reversed(TABLE_SUFFIXES)
        )
        self.execute(
            f"""
            {drops}
            DROP FUNCTION IF EXISTS {prefix}idempotency_acct_fn();
            DROP FUNCTION IF EXISTS {prefix}run_events_acct_fn();
            """
        )
        escaped = prefix.replace("_", "\\_")
        remaining = self.json(
            f"""
            SELECT json_build_object(
                'relations', (SELECT COUNT(*)::bigint FROM pg_class
                    WHERE relnamespace = (SELECT oid FROM pg_namespace
                                           WHERE nspname = current_schema())
                      AND relname LIKE '{escaped}%' ESCAPE '\\'),
                'functions', (SELECT COUNT(*)::bigint FROM pg_proc
                    WHERE pronamespace = (SELECT oid FROM pg_namespace
                                           WHERE nspname = current_schema())
                      AND proname LIKE '{escaped}%' ESCAPE '\\')
            )::text;
            """
        )
        if remaining != {"relations": 0, "functions": 0}:
            raise RuntimeError(f"prefix cleanup left resources: {remaining}")
        return {"prefix": prefix, "remaining_relations": 0, "remaining_functions": 0}
