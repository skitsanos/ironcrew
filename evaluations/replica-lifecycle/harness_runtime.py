"""Process, HTTP, RSS, and SSE primitives for the IC-020 local gate."""

from __future__ import annotations

import json
import os
import signal
import socket
import subprocess
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any


MAX_RESPONSE_BYTES = 1024 * 1024


def free_loopback_port() -> int:
    with socket.socket() as candidate:
        candidate.bind(("127.0.0.1", 0))
        return int(candidate.getsockname()[1])


def request(
    method: str,
    url: str,
    token: str | None = None,
    payload: Any | None = None,
    headers: dict[str, str] | None = None,
    timeout: float = 10.0,
) -> tuple[int, bytes, dict[str, str]]:
    request_headers = {"Accept": "application/json", "User-Agent": "ic020-capacity/1"}
    if token:
        request_headers["Authorization"] = f"Bearer {token}"
    if headers:
        request_headers.update(headers)
    body = None
    if payload is not None:
        body = json.dumps(payload, separators=(",", ":")).encode()
        request_headers["Content-Type"] = "application/json"
    candidate = urllib.request.Request(url, data=body, headers=request_headers, method=method)
    try:
        response = urllib.request.urlopen(candidate, timeout=timeout)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        response_body = response.read(MAX_RESPONSE_BYTES + 1)
        if len(response_body) > MAX_RESPONSE_BYTES:
            raise RuntimeError("HTTP response exceeded one MiB")
        return response.status, response_body, dict(response.headers)


def request_json(*args: Any, **kwargs: Any) -> tuple[int, Any, dict[str, str]]:
    status, body, headers = request(*args, **kwargs)
    return status, json.loads(body) if body else None, headers


def wait_ready(base_url: str, process: subprocess.Popen[bytes], timeout: float = 20.0) -> None:
    deadline = time.monotonic() + timeout
    last_status = None
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"replica exited during startup with code {process.returncode}")
        try:
            last_status, _, _ = request("GET", f"{base_url}/health/ready", timeout=1)
            if last_status == 200:
                return
        except (OSError, TimeoutError):
            pass
        time.sleep(0.1)
    raise TimeoutError(f"replica readiness timed out; last status={last_status}")


class ReplicaSet:
    def __init__(self, root: Path, binary: Path, flows: Path, logs: Path) -> None:
        self.root = root
        self.binary = binary
        self.flows = flows
        self.logs = logs
        self.processes: dict[str, subprocess.Popen[bytes]] = {}
        self.bases: dict[str, str] = {}
        self._log_handles: list[Any] = []

    def start(self, name: str, environment: dict[str, str]) -> None:
        port = free_loopback_port()
        base_url = f"http://127.0.0.1:{port}"
        log_handle = (self.logs / f"{name}.log").open("wb")
        process = subprocess.Popen(
            [
                str(self.binary),
                "serve",
                "--host",
                "127.0.0.1",
                "--port",
                str(port),
                "--flows-dir",
                str(self.flows),
            ],
            cwd=self.root,
            env=environment,
            stdout=log_handle,
            stderr=subprocess.STDOUT,
            start_new_session=os.name == "posix",
        )
        self.processes[name] = process
        self.bases[name] = base_url
        self._log_handles.append(log_handle)
        wait_ready(base_url, process)

    def assert_alive(self) -> None:
        exited = {name: process.returncode for name, process in self.processes.items() if process.poll() is not None}
        if exited:
            raise RuntimeError(f"replicas exited before controlled shutdown: {exited}")

    def stop_all(self, timeout: float = 15.0) -> dict[str, dict[str, Any]]:
        for process in self.processes.values():
            if process.poll() is None:
                try:
                    if os.name == "posix":
                        os.killpg(process.pid, signal.SIGTERM)
                    else:
                        process.terminate()
                except ProcessLookupError:
                    pass
        deadline = time.monotonic() + timeout
        outcomes: dict[str, dict[str, Any]] = {}
        for name, process in self.processes.items():
            forced = False
            try:
                process.wait(timeout=max(0.0, deadline - time.monotonic()))
            except subprocess.TimeoutExpired:
                forced = True
                if os.name == "posix":
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                else:
                    process.kill()
                process.wait(timeout=5)
            outcomes[name] = {"exit_code": process.returncode, "forced_kill": forced}
        for handle in self._log_handles:
            handle.close()
        self._log_handles.clear()
        return outcomes


def process_rss_bytes(pid: int) -> tuple[int, str]:
    status = Path(f"/proc/{pid}/status")
    if status.is_file():
        for line in status.read_text(encoding="utf-8").splitlines():
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) * 1024, "proc_status_vmrss"
    result = subprocess.run(
        ["ps", "-o", "rss=", "-p", str(pid)],
        check=True,
        capture_output=True,
        text=True,
        timeout=5,
    )
    return int(result.stdout.strip()) * 1024, "ps_rss"


def sample_rss(
    processes: dict[str, subprocess.Popen[bytes]], samples: int = 8, interval: float = 0.1
) -> dict[str, Any]:
    peaks = {name: 0 for name in processes}
    aggregate_peak = 0
    source = "unknown"
    for index in range(samples):
        aggregate = 0
        for name, process in processes.items():
            rss, source = process_rss_bytes(process.pid)
            peaks[name] = max(peaks[name], rss)
            aggregate += rss
        aggregate_peak = max(aggregate_peak, aggregate)
        if index + 1 < samples:
            time.sleep(interval)
    return {
        "source": source,
        "samples": samples,
        "per_process_peak_bytes": peaks,
        "aggregate_peak_bytes": aggregate_peak,
        "platform_or_cgroup_measurement": False,
    }


def parse_metric(body: str, metric: str) -> int:
    values = []
    for line in body.splitlines():
        if line.startswith(f"{metric} "):
            values.append(int(float(line.split()[-1])))
    if len(values) != 1:
        raise RuntimeError(f"expected one unlabelled {metric} sample, found {len(values)}")
    return values[0]


def replica_metrics(base_url: str, token: str) -> dict[str, int]:
    status, body, _ = request("GET", f"{base_url}/metrics", token=token)
    if status != 200:
        raise RuntimeError(f"metrics returned HTTP {status}")
    text = body.decode()
    names = (
        "ironcrew_process_active_runs",
        "ironcrew_process_active_runs_limit",
        "ironcrew_process_active_sse_connections",
        "ironcrew_process_active_sse_connections_limit",
        "ironcrew_process_memory_measurement_available",
        "ironcrew_postgres_pool_open_connections",
        "ironcrew_postgres_pool_in_use_connections",
        "ironcrew_postgres_pool_connections_limit",
        "ironcrew_process_active_provider_calls",
        "ironcrew_process_peak_active_provider_calls",
        "ironcrew_process_eventbus_instances",
        "ironcrew_process_eventbus_retained_events",
        "ironcrew_process_eventbus_retained_bytes",
        "ironcrew_process_eventbus_retained_events_capacity",
        "ironcrew_process_eventbus_retained_bytes_capacity",
    )
    metrics = {name: parse_metric(text, name) for name in names}
    if metrics["ironcrew_process_memory_measurement_available"] == 1:
        for name in (
            "ironcrew_process_resident_memory_bytes",
            "ironcrew_process_peak_resident_memory_bytes",
        ):
            metrics[name] = parse_metric(text, name)
    return metrics


class SseHandle:
    def __init__(self, url: str, token: str) -> None:
        self.url = url
        self.token = token
        self.ready = threading.Event()
        self.status: int | None = None
        self.error: str | None = None
        self._response: Any | None = None
        self._thread = threading.Thread(target=self._run, daemon=True)

    def start(self) -> None:
        self._thread.start()
        if not self.ready.wait(10):
            raise TimeoutError("SSE response headers timed out")
        if self.status != 200:
            raise RuntimeError(f"SSE failed before saturation: status={self.status} error={self.error}")

    def _run(self) -> None:
        candidate = urllib.request.Request(
            self.url,
            headers={
                "Accept": "text/event-stream",
                "Authorization": f"Bearer {self.token}",
                "User-Agent": "ic020-capacity/1",
            },
        )
        try:
            self._response = urllib.request.urlopen(candidate, timeout=20)
            self.status = self._response.status
            self.ready.set()
            while self._response.readline():
                pass
        except urllib.error.HTTPError as error:
            self.status = error.code
            self.error = f"http_{error.code}"
            self.ready.set()
        except (OSError, TimeoutError) as error:
            self.error = type(error).__name__
            self.ready.set()
        finally:
            if self._response is not None:
                self._response.close()

    def wait_closed(self, timeout: float = 20.0) -> None:
        self._thread.join(timeout)
        if self._thread.is_alive():
            raise TimeoutError("SSE connection did not close after terminal run")


def extra_sse_status(url: str, token: str) -> int:
    candidate = urllib.request.Request(
        url,
        headers={"Accept": "text/event-stream", "Authorization": f"Bearer {token}"},
    )
    try:
        response = urllib.request.urlopen(candidate, timeout=5)
    except urllib.error.HTTPError as error:
        return error.code
    with response:
        return response.status
