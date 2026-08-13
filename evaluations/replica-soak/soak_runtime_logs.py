"""Bounded, non-retaining subprocess log evidence for replica soak."""

from __future__ import annotations

import hashlib
import os
import signal
import subprocess
import threading
import time
from pathlib import Path
from typing import Any, BinaryIO

from soak_runtime_directory import ExternalRuntimeDirectory


READ_CHUNK_BYTES = 64 * 1024
MAX_CANARY_BYTES = 64 * 1024
MAX_CANARIES = 32


def validate_canaries(values: dict[str, str]) -> dict[str, bytes]:
    """Validate and encode named secrets without exposing their values."""
    if len(values) > MAX_CANARIES:
        raise ValueError(f"runtime log scan accepts at most {MAX_CANARIES} canaries")
    output: dict[str, bytes] = {}
    for label, value in values.items():
        if not isinstance(label, str) or not label or len(label) > 64:
            raise ValueError("runtime log canary labels must contain 1..=64 characters")
        if not isinstance(value, str):
            raise ValueError(f"runtime log canary {label!r} must be a string")
        if not value:
            continue
        encoded = value.encode("utf-8")
        if len(encoded) > MAX_CANARY_BYTES:
            raise ValueError(
                f"runtime log canary {label!r} exceeds {MAX_CANARY_BYTES} bytes"
            )
        output[label] = encoded
    return output


class StreamingLogCollector:
    """Drain one byte stream while retaining only counters, a digest, and hits."""

    def __init__(
        self, name: str, stream: BinaryIO, canaries: dict[str, str]
    ) -> None:
        self.name = name
        self.stream = stream
        self.canaries = validate_canaries(canaries)
        self.maximum_canary_bytes = max(
            (len(value) for value in self.canaries.values()), default=0
        )
        self.digest = hashlib.sha256()
        self.observed_bytes = 0
        self.detected: set[str] = set()
        self.completed = False
        self.error: str | None = None
        self.thread = threading.Thread(
            target=self._drain,
            name=f"soak-log-{name}",
            daemon=True,
        )

    def start(self) -> None:
        self.thread.start()

    def _drain(self) -> None:
        overlap = b""
        overlap_bytes = max(0, self.maximum_canary_bytes - 1)
        try:
            while chunk := self.stream.read(READ_CHUNK_BYTES):
                self.observed_bytes += len(chunk)
                self.digest.update(chunk)
                scan = overlap + chunk
                for label, canary in self.canaries.items():
                    if label not in self.detected and canary in scan:
                        self.detected.add(label)
                overlap = scan[-overlap_bytes:] if overlap_bytes else b""
            self.completed = True
        except BaseException as error:  # the receipt must expose drain failures
            self.error = f"log stream drain failed ({type(error).__name__})"
        finally:
            try:
                self.stream.close()
            except Exception as error:
                if self.error is None:
                    self.error = f"log stream close failed ({type(error).__name__})"

    def finish(self, timeout_seconds: float = 5.0) -> dict[str, Any]:
        self.thread.join(timeout=timeout_seconds)
        if self.thread.is_alive():
            self.error = "log collector thread did not stop"
            return {
                "observed_bytes": None,
                "sha256": None,
                "drain_completed": False,
                "secret_scan_passed": False,
                "canary_labels_checked": sorted(self.canaries),
                "canary_labels_detected": [],
                "raw_content_retained": False,
                "maximum_scan_buffer_bytes": READ_CHUNK_BYTES
                + max(0, self.maximum_canary_bytes - 1),
                "error": self.error,
            }
        drain_completed = self.completed and self.error is None
        secret_scan_passed = drain_completed and not self.detected
        return {
            "observed_bytes": self.observed_bytes,
            "sha256": self.digest.hexdigest(),
            "drain_completed": drain_completed,
            "secret_scan_passed": secret_scan_passed,
            "canary_labels_checked": sorted(self.canaries),
            "canary_labels_detected": sorted(self.detected),
            "raw_content_retained": False,
            "maximum_scan_buffer_bytes": READ_CHUNK_BYTES
            + max(0, self.maximum_canary_bytes - 1),
            "error": self.error,
        }


def runtime_log_criterion(
    replicas: dict[str, dict[str, Any]], *, applicable: bool
) -> dict[str, Any]:
    """Fail closed when either launched replica lacks complete safe log evidence."""
    if not applicable:
        return {
            "applicable": False,
            "passed": None,
            "status": "not_applicable",
            "replicas": replicas,
        }
    complete_set = set(replicas) == {"a", "b"}
    passed = complete_set and all(
        item.get("drain_completed") is True
        and item.get("secret_scan_passed") is True
        and item.get("raw_content_retained") is False
        and item.get("error") is None
        for item in replicas.values()
    )
    return {
        "applicable": True,
        "passed": passed,
        "status": "passed" if passed else "failed",
        "expected_replicas": ["a", "b"],
        "observed_replicas": sorted(replicas),
        "replicas": replicas,
    }


class ReplicaLauncher:
    """Launch and stop local replicas with non-retaining log collectors."""

    def __init__(self, source_root: Path, flow_root: Path) -> None:
        self.runtime_directory = ExternalRuntimeDirectory(source_root)
        self.flow_root = flow_root
        self.processes: dict[str, subprocess.Popen[bytes]] = {}
        self.log_collectors: dict[str, StreamingLogCollector] = {}
        self.log_canaries: dict[str, str] = {}
        self.log_canaries_configured = False
        self.log_evidence: dict[str, dict[str, Any]] = {}

    def configure_log_canaries(self, values: dict[str, str]) -> None:
        if self.processes:
            raise RuntimeError("runtime log canaries must be configured before launch")
        validate_canaries(values)
        self.log_canaries = dict(values)
        self.log_canaries_configured = True

    def start(
        self,
        name: str,
        binary: Path,
        host: str,
        port: int,
        environment: dict[str, str],
    ) -> subprocess.Popen[bytes]:
        if not self.log_canaries_configured:
            raise RuntimeError("runtime log canaries were not configured")
        if os.path.lexists(self.flow_root / ".env"):
            raise RuntimeError("replica soak refuses a flow root containing `.env`")
        runtime_dir = self.runtime_directory.ensure()
        try:
            process = subprocess.Popen(
                [
                    str(binary),
                    "serve",
                    "--host",
                    host,
                    "--port",
                    str(port),
                    "--flows-dir",
                    str(self.flow_root),
                ],
                cwd=runtime_dir,
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                start_new_session=os.name == "posix",
            )
        except BaseException:
            if not self.processes:
                self.runtime_directory.cleanup()
            raise
        if process.stdout is None:
            process.kill()
            process.wait(timeout=5)
            if not self.processes:
                self.runtime_directory.cleanup()
            raise RuntimeError("replica log pipe was not created")
        collector = StreamingLogCollector(name, process.stdout, self.log_canaries)
        collector.start()
        self.processes[name] = process
        self.log_collectors[name] = collector
        return process

    def stop_all(self, grace_seconds: float = 10.0) -> dict[str, Any]:
        outcome: dict[str, Any] = {}
        for process in self.processes.values():
            if process.poll() is None:
                try:
                    if os.name == "posix":
                        os.killpg(process.pid, signal.SIGTERM)
                    else:
                        process.terminate()
                except ProcessLookupError:
                    pass
        deadline = time.monotonic() + grace_seconds
        try:
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
                outcome[name] = {
                    "exit_code": process.returncode,
                    "forced_kill": forced,
                }
        finally:
            try:
                self.log_evidence = {
                    name: collector.finish()
                    for name, collector in sorted(self.log_collectors.items())
                }
            finally:
                self.runtime_directory.cleanup()
        return outcome
