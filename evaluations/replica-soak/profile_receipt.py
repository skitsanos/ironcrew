"""Final provenance and pass/fail assembly for IC-018 profile receipts."""

from __future__ import annotations

import hashlib
from pathlib import Path
from typing import Any

from source_provenance import worktree_provenance


EXPECTED_MOCK_COUNTS = {
    "provider_tool_provider_calls": 2,
    "provider_tool_tool_effects": 1,
    "large_result_provider_calls": 1,
    "conversation_provider_calls": 2,
    "invalid_requests": 0,
}
EXPECTED_PROFILE_IDS = {
    "provider_tool",
    "large_result",
    "owner_local_conversation_boundary",
}
EXPECTED_INSTANCE_IDS = ["ic018-profile-a", "ic018-profile-b"]


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def capture_source(root: Path, binary: Path) -> dict[str, Any]:
    """Bind the source tree and exact executable at one receipt boundary."""
    return {**worktree_provenance(root), "binary_sha256": _sha256(binary)}


def _conversation_contract(profiles: list[dict[str, Any]]) -> bool:
    candidates = [
        profile
        for profile in profiles
        if profile.get("id") == "owner_local_conversation_boundary"
    ]
    if len(candidates) != 1:
        return False
    profile = candidates[0]
    evidence = profile.get("evidence") or {}
    identity = evidence.get("identity") or {}
    revisions = evidence.get("revisions")
    revision_contract = (
        isinstance(revisions, list)
        and len(revisions) == 4
        and all(
            isinstance(revision, int) and not isinstance(revision, bool)
            for revision in revisions
        )
        and revisions[0] >= 0
        and revisions[1] == revisions[0] + 1
        and revisions[2] == revisions[1] + 1
        and revisions[3] == revisions[2]
    )
    return (
        profile.get("status") == "passed"
        and revision_contract
        and evidence.get("in_flight_takeover_proven") is False
        and identity.get("conversation_id") == evidence.get("conversation_id")
        and isinstance(identity.get("incarnation_id"), str)
        and isinstance(identity.get("definition_fingerprint"), str)
    )


def finalize_report(
    report: dict[str, Any],
    source_at_start: dict[str, Any],
    source_after_cleanup: dict[str, Any] | None,
    source_capture_error: str | None = None,
) -> int:
    """Assemble the only final verdict, after cleanup and final provenance capture."""
    source_unchanged = (
        source_capture_error is None
        and source_after_cleanup is not None
        and source_at_start == source_after_cleanup
    )
    report["source"] = {
        **source_at_start,
        "capture_boundary": "before database and process work",
        "after_cleanup": source_after_cleanup,
        "unchanged_during_run": source_unchanged,
        "final_capture_error": source_capture_error,
    }

    profiles = report.get("profiles")
    profile_rows = profiles if isinstance(profiles, list) else []
    shutdown = report.get("shutdown")
    shutdown_rows = shutdown if isinstance(shutdown, dict) else {}
    cleanup = report.get("cleanup")
    cleanup_row = cleanup if isinstance(cleanup, dict) else {}
    topology = report.get("topology")
    topology_row = topology if isinstance(topology, dict) else {}
    mock = report.get("mock_provider")
    mock_row = mock if isinstance(mock, dict) else {}

    criteria = {
        "execution_completed": "error" not in report,
        "profile_set_complete": {
            row.get("id") for row in profile_rows if isinstance(row, dict)
        } == EXPECTED_PROFILE_IDS,
        "profiles_passed": bool(profile_rows)
        and all(row.get("status") == "passed" for row in profile_rows),
        "conversation_identity_revision_contract": _conversation_contract(profile_rows),
        "exact_mock_activity": mock_row.get("counts") == EXPECTED_MOCK_COUNTS,
        "replica_identity": topology_row.get("instance_ids") == EXPECTED_INSTANCE_IDS,
        "replica_shutdown_complete": set(shutdown_rows) == {"a", "b"},
        "process_exit_zero": bool(shutdown_rows)
        and all(row.get("exit_code") == 0 for row in shutdown_rows.values()),
        "no_forced_kill": bool(shutdown_rows)
        and all(row.get("forced_kill") is False for row in shutdown_rows.values()),
        "cleanup_performed": cleanup_row.get("performed") is True
        and cleanup_row.get("error") is None,
        "zero_prefix_objects": cleanup_row.get("zero_verified") is True
        and cleanup_row.get("remaining_objects") == 0,
        "runtime_log_secret_scan": "runtime_logs" in report,
        "source_provenance_unchanged": source_unchanged,
    }
    criteria["overall_passed"] = all(criteria.values())
    report["pass_criteria"] = criteria
    report["status"] = "passed" if criteria["overall_passed"] else "failed"
    return 0 if criteria["overall_passed"] else 1
