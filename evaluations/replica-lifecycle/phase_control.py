"""Run admission, terminal reads, and bounded phase cleanup."""

from __future__ import annotations

import secrets
import time

from capacity_config import ACTIVE_RUNS_PER_REPLICA
from harness_runtime import ReplicaSet, replica_metrics, request_json


def submit_runs(replicas: ReplicaSet, token: str, phase: int) -> dict[str, list[str]]:
    runs: dict[str, list[str]] = {}
    for name, base_url in replicas.bases.items():
        runs[name] = []
        for index in range(ACTIVE_RUNS_PER_REPLICA):
            status, payload, _ = request_json(
                "POST",
                f"{base_url}/flows/capacity/run",
                token=token,
                payload={"phase": phase, "slot": index},
                headers={"Idempotency-Key": f"ic020-{phase}-{name}-{index}-{secrets.token_hex(8)}"},
            )
            if status != 200 or payload.get("status") != "started":
                raise RuntimeError(f"run admission failed with HTTP {status}")
            runs[name].append(payload["run_id"])
    return runs


def wait_terminal(replicas: ReplicaSet, token: str, runs: dict[str, list[str]]) -> None:
    deadline = time.monotonic() + 20
    pending = {(name, run_id) for name, values in runs.items() for run_id in values}
    while pending and time.monotonic() < deadline:
        for name, run_id in tuple(pending):
            status, payload, _ = request_json(
                "GET", f"{replicas.bases[name]}/flows/capacity/runs/{run_id}", token=token
            )
            run_status = str(payload.get("status", "")).lower() if status == 200 else ""
            if run_status in {"success", "completed"}:
                pending.remove((name, run_id))
            elif status != 200:
                raise RuntimeError(f"terminal run read returned HTTP {status}")
            elif run_status not in {"running", "waiting_forinput", "waiting_for_input"}:
                raise RuntimeError(f"run terminalized as {run_status or 'unknown'}")
        if pending:
            time.sleep(0.1)
    if pending:
        raise TimeoutError(f"{len(pending)} runs did not complete")


def wait_quiescent(
    replicas: ReplicaSet, token: str, timeout: float = 5.0
) -> dict[str, object]:
    started = time.monotonic()
    deadline = started + timeout
    zero_names = (
        "ironcrew_process_active_runs",
        "ironcrew_process_active_sse_connections",
        "ironcrew_process_active_provider_calls",
        "ironcrew_process_eventbus_instances",
        "ironcrew_process_eventbus_retained_events",
        "ironcrew_process_eventbus_retained_bytes",
        "ironcrew_process_eventbus_retained_events_capacity",
        "ironcrew_process_eventbus_retained_bytes_capacity",
    )
    latest: dict[str, dict[str, int]] = {}
    while time.monotonic() < deadline:
        latest = {
            name: replica_metrics(base_url, token)
            for name, base_url in replicas.bases.items()
        }
        if all(metrics[name] == 0 for metrics in latest.values() for name in zero_names):
            return {
                "elapsed_ms": round((time.monotonic() - started) * 1000, 3),
                "required_zero_metrics": list(zero_names),
                "metrics_by_replica": latest,
            }
        time.sleep(0.05)
    raise TimeoutError(f"phase resources did not quiesce within {timeout}s: {latest}")
