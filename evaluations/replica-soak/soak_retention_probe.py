"""Explicit delayed replay and exact-prefix cleanup probes."""

from __future__ import annotations

import json
import re
import uuid
from typing import Any


PREFIX_PATTERN = re.compile(r"[a-z][a-z0-9_]{2,31}")


def _prefix(value: str) -> str:
    if not PREFIX_PATTERN.fullmatch(value):
        raise ValueError("invalid replica-soak table prefix")
    return value


def _run_id(value: str) -> str:
    parsed = uuid.UUID(value)
    if str(parsed) != value:
        raise ValueError("run id must be a canonical UUID")
    return value


def retention_anchor_sql(prefix: str, run_id: str) -> str:
    prefix, run_id = _prefix(prefix), _run_id(run_id)
    return f"""
SELECT json_build_object(
    'physical_rows', (
        SELECT COUNT(*)::bigint FROM {prefix}run_events WHERE run_id = '{run_id}'
    ),
    'retained_events', COALESCE((
        SELECT retained_events FROM {prefix}run_event_state WHERE run_id = '{run_id}'
    ), 0)::bigint,
    'dropped_through', COALESCE((
        SELECT dropped_through FROM {prefix}run_event_state WHERE run_id = '{run_id}'
    ), 0)::bigint,
    'eviction_reason', (
        SELECT eviction_reason FROM {prefix}run_event_state WHERE run_id = '{run_id}'
    )
)::text;
"""


def post_cleanup_inventory_sql(prefix: str) -> str:
    prefix = _prefix(prefix)
    return f"""
SELECT json_build_object(
    'relations', (
        SELECT COUNT(*)::bigint
          FROM pg_class class
          JOIN pg_namespace namespace ON namespace.oid = class.relnamespace
         WHERE namespace.nspname = current_schema()
           AND left(class.relname, length('{prefix}')) = '{prefix}'
    ),
    'functions', (
        SELECT COUNT(*)::bigint
          FROM pg_proc function
          JOIN pg_namespace namespace ON namespace.oid = function.pronamespace
         WHERE namespace.nspname = current_schema()
           AND left(function.proname, length('{prefix}')) = '{prefix}'
    )
)::text;
"""


def _error_code(response: Any) -> str | None:
    try:
        value = json.loads(response.body)
    except (AttributeError, UnicodeDecodeError, json.JSONDecodeError):
        return None
    if not isinstance(value, dict):
        return None
    error = value.get("error")
    if isinstance(error, dict) and isinstance(error.get("code"), str):
        return error["code"]
    return value.get("code") if isinstance(value.get("code"), str) else None


def _event_payload(event: dict[str, Any]) -> Any:
    payload = event.get("data")
    if (
        isinstance(payload, dict)
        and payload.get("event") == event.get("event")
        and "data" in payload
    ):
        return payload["data"]
    return payload


def delayed_replay_probe(
    results: list[dict[str, Any]],
    bases: tuple[str, str],
    client: Any,
    postgres: Any,
    prefix: str,
    max_sse_bytes: int,
) -> dict[str, Any]:
    selected = next(
        (
            item
            for item in sorted(results, key=lambda value: value.get("index", 0))
            if item.get("success")
            and isinstance(item.get("_replay_cursor"), str)
            and isinstance(item.get("run_id"), str)
        ),
        None,
    )
    if selected is None:
        return {"status": "not_available", "reason": "no successful replay anchor"}
    peer_index = 0 if selected.get("peer_replica") == "a" else 1
    run_id, cursor = selected["run_id"], selected["_replay_cursor"]
    url = f"{bases[peer_index]}/flows/soak/events/{run_id}"
    report: dict[str, Any] = {
        "status": "completed",
        "anchor_run_id": run_id,
        "anchor": postgres.json(retention_anchor_sql(prefix, run_id)),
    }
    try:
        response = client.request(
            "replay_expired_cursor_probe",
            "GET",
            url,
            headers={"Accept": "text/event-stream", "Last-Event-ID": cursor},
        )
        report["cursor_probe"] = {
            "status": response.status,
            "code": _error_code(response),
        }
    except Exception as error:
        report["cursor_probe"] = {
            "status": None,
            "code": None,
            "error": f"{type(error).__name__}: {str(error)[:300]}",
        }
    try:
        stream = client.sse_until(
            "replay_cursorless_gap_probe",
            url,
            "run_complete",
            max_bytes=max_sse_bytes,
        )
        terminal = _event_payload(stream)
        gaps = stream.get("journal_gaps", [])
        report["gap_probe"] = {
            "count": len(gaps),
            "reasons": sorted(
                {
                    gap.get("reason")
                    for gap in gaps
                    if isinstance(gap, dict) and isinstance(gap.get("reason"), str)
                }
            ),
            "gaps": gaps,
            "terminal": {
                "id": stream.get("id"),
                "status": terminal.get("status") if isinstance(terminal, dict) else None,
                "journal_complete": (
                    terminal.get("journal_complete") if isinstance(terminal, dict) else None
                ),
                "synthesized_from_run_record": (
                    terminal.get("synthesized_from_run_record")
                    if isinstance(terminal, dict)
                    else None
                ),
            },
        }
    except Exception as error:
        report["gap_probe"] = {
            "count": 0,
            "reasons": [],
            "error": f"{type(error).__name__}: {str(error)[:300]}",
        }
    return report
