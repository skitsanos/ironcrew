"""Bounded IC-018 profile workloads and machine-readable result contracts."""

from __future__ import annotations

import hashlib
import time
import uuid
from collections.abc import Callable
from typing import Any, Protocol

from profile_contract import profile_metadata
from profile_conversation_contract import validate_conversation_state
from profile_provider import (
    CONVERSATION_OUTPUT,
    LARGE_RESULT_BYTES,
    PROVIDER_TOOL_OUTPUT,
)


SHARED_STORE_SSE_ERROR = (
    "Conversation SSE replay is unavailable with shared-store coordination; "
    "use durable history for recovery"
)


class ProfileError(RuntimeError):
    """A bounded profile contract failed."""


class JsonResponse(Protocol):
    status: int

    def json(self) -> Any: ...


class ProfileClient(Protocol):
    def request(
        self,
        operation: str,
        method: str,
        url: str,
        payload: Any | None = None,
        headers: dict[str, str] | None = None,
    ) -> JsonResponse: ...


def _body(response: JsonResponse, expected_status: int, label: str) -> dict[str, Any]:
    if response.status != expected_status:
        raise ProfileError(f"{label} returned HTTP {response.status}")
    value = response.json()
    if not isinstance(value, dict):
        raise ProfileError(f"{label} returned a non-object response")
    return value


def _digest(value: str) -> str:
    return f"sha256:{hashlib.sha256(value.encode()).hexdigest()}"


def _run_output(
    client: ProfileClient,
    *,
    flow: str,
    start_base: str,
    read_base: str,
    timeout_seconds: float,
    poll_interval_seconds: float,
) -> tuple[str, str, int]:
    key = f"ic018-profile-{uuid.uuid4().hex}"
    started = _body(
        client.request(
            f"{flow}_start",
            "POST",
            f"{start_base}/flows/{flow}/run",
            {},
            {"Idempotency-Key": key},
        ),
        200,
        f"{flow} start",
    )
    run_id = started.get("run_id")
    if not isinstance(run_id, str) or not run_id:
        raise ProfileError(f"{flow} start omitted run_id")
    deadline = time.monotonic() + timeout_seconds
    polls = 0
    while time.monotonic() < deadline:
        polls += 1
        response = client.request(
            f"{flow}_read", "GET", f"{read_base}/flows/{flow}/runs/{run_id}"
        )
        if response.status == 200:
            record = _body(response, 200, f"{flow} read")
            status = str(record.get("status", "")).lower()
            if status == "success":
                results = record.get("task_results")
                if not isinstance(results, list) or len(results) != 1:
                    raise ProfileError(f"{flow} terminal result count is invalid")
                output = results[0].get("output") if isinstance(results[0], dict) else None
                if not isinstance(output, str):
                    raise ProfileError(f"{flow} terminal output is invalid")
                return run_id, output, polls
            if status in {"abandoned", "aborted", "timedout", "timed_out", "failed"}:
                raise ProfileError(f"{flow} reached terminal status {status}")
        elif response.status != 404:
            raise ProfileError(f"{flow} read returned HTTP {response.status}")
        time.sleep(poll_interval_seconds)
    raise ProfileError(f"{flow} did not complete within the profile timeout")


def provider_tool_profile(
    bases: tuple[str, str], client: ProfileClient, timeout: float, poll: float
) -> dict[str, Any]:
    run_id, output, polls = _run_output(
        client,
        flow="profile-provider-tool",
        start_base=bases[0],
        read_base=bases[1],
        timeout_seconds=timeout,
        poll_interval_seconds=poll,
    )
    if output != PROVIDER_TOOL_OUTPUT:
        raise ProfileError("provider/tool output did not match the fixed fixture receipt")
    return {
        "run_id": run_id,
        "route": "execution_owner_a_terminal_read_peer_b",
        "terminal_read_scope": "shared_postgresql_record",
        "polls": polls,
        "output_bytes": len(output.encode()),
        "output_sha256": _digest(output),
    }


def large_result_profile(
    bases: tuple[str, str], client: ProfileClient, timeout: float, poll: float
) -> dict[str, Any]:
    run_id, output, polls = _run_output(
        client,
        flow="profile-large-result",
        start_base=bases[1],
        read_base=bases[0],
        timeout_seconds=timeout,
        poll_interval_seconds=poll,
    )
    encoded = output.encode()
    if encoded != b"L" * LARGE_RESULT_BYTES:
        raise ProfileError("large-result output did not match the fixed byte contract")
    return {
        "run_id": run_id,
        "route": "execution_owner_b_terminal_read_peer_a",
        "terminal_read_scope": "shared_postgresql_record",
        "polls": polls,
        "output_bytes": len(encoded),
        "output_sha256": _digest(output),
        "raw_output_retained_in_report": False,
    }


def conversation_profile(
    bases: tuple[str, str], client: ProfileClient, _timeout: float, _poll: float
) -> dict[str, Any]:
    conversation_id = f"ic018-{uuid.uuid4().hex}"
    root = f"/flows/profile-conversation/conversations/{conversation_id}"
    started = _body(
        client.request(
            "conversation_start_owner",
            "POST",
            f"{bases[0]}{root}/start",
            {"agent": "coordinator", "max_history": 8},
        ),
        200,
        "conversation start",
    )
    first = _body(
        client.request(
            "conversation_message_warm_owner",
            "POST",
            f"{bases[0]}{root}/messages",
            {"content": "bounded warm-owner turn"},
            {"Idempotency-Key": f"ic018-warm-{uuid.uuid4().hex}"},
        ),
        200,
        "warm-owner conversation turn",
    )
    second = _body(
        client.request(
            "conversation_message_cold_peer",
            "POST",
            f"{bases[1]}{root}/messages",
            {"content": "bounded cold-peer turn"},
            {"Idempotency-Key": f"ic018-cold-{uuid.uuid4().hex}"},
        ),
        200,
        "cold-peer conversation turn",
    )
    history = _body(
        client.request(
            "conversation_history_owner", "GET", f"{bases[0]}{root}/history"
        ),
        200,
        "conversation history",
    )
    boundary = client.request(
        "conversation_sse_shared_store_boundary",
        "GET",
        f"{bases[1]}{root}/events",
        None,
        {"Last-Event-ID": "ic018-profile-cursor"},
    )
    boundary_body = _body(boundary, 409, "shared-store conversation SSE boundary")
    deleted = _body(
        client.request("conversation_delete_peer", "DELETE", f"{bases[1]}{root}"),
        200,
        "conversation delete",
    )
    assistants = (first.get("assistant"), second.get("assistant"))
    state = validate_conversation_state(conversation_id, started, first, second, history)
    if assistants != (CONVERSATION_OUTPUT, CONVERSATION_OUTPUT):
        raise ProfileError("conversation replies did not match the mock contract")
    if boundary_body.get("error") != SHARED_STORE_SSE_ERROR:
        raise ProfileError("conversation SSE 409 was not the shared-store boundary")
    if deleted.get("deleted") != conversation_id:
        raise ProfileError("conversation delete receipt was inconsistent")
    return {
        "conversation_id": conversation_id,
        "turns": 2,
        "history_messages": sum(state["roles"].values()),
        "history_role_counts": state["roles"],
        "assistant_output_sha256": _digest(CONVERSATION_OUTPUT),
        "identity": state["identity"],
        "revisions": state["revisions"],
        "steps": [
            {"operation": "start", "route": "warm_owner_a", "status": 200},
            {"operation": "message_1", "route": "warm_owner_a", "status": 200},
            {
                "operation": "message_2",
                "route": "cold_peer_b_committed_boundary_rehydration",
                "status": 200,
            },
            {"operation": "history", "route": "shared_store_via_owner_a", "status": 200},
            {
                "operation": "conversation_sse",
                "route": "shared_store_unsupported_via_peer_b",
                "status": 409,
                "error_sha256": _digest(SHARED_STORE_SSE_ERROR),
            },
            {"operation": "delete", "route": "peer_b", "status": 200},
        ],
        "in_flight_takeover_proven": False,
    }


def _counter_delta(before: dict[str, int], after: dict[str, int]) -> dict[str, int]:
    return {name: after.get(name, 0) - before.get(name, 0) for name in sorted(after)}


def run_profile_suite(
    bases: tuple[str, str],
    client: ProfileClient,
    counter_snapshot: Callable[[], dict[str, int]],
    *,
    timeout_seconds: float = 30.0,
    poll_interval_seconds: float = 0.1,
    sanitize_error: Callable[[BaseException], str] = lambda error: str(error)[:500],
) -> list[dict[str, Any]]:
    functions = (provider_tool_profile, large_result_profile, conversation_profile)
    results = []
    for spec, function in zip(profile_metadata(), functions, strict=True):
        before = counter_snapshot()
        try:
            evidence = function(bases, client, timeout_seconds, poll_interval_seconds)
            error = None
        except Exception as caught:  # keep independent profiles observable
            evidence = {}
            error = {"kind": type(caught).__name__, "message": sanitize_error(caught)}
        activity = _counter_delta(before, counter_snapshot())
        expected = spec["expected_mock_activity"]
        mock_activity_passed = all(activity.get(name) == value for name, value in expected.items())
        results.append(
            {
                **spec,
                "status": "passed" if error is None and mock_activity_passed else "failed",
                "mock_activity": activity,
                "mock_activity_passed": mock_activity_passed,
                "evidence": evidence,
                "error": error,
            }
        )
    return results
