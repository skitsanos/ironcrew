"""Local process and secret-boundary helpers for IC-018 mock profiles."""

from __future__ import annotations

import os
import signal
import socket
import subprocess
import time
from pathlib import Path
from typing import Any


MAX_LOG_BYTES = 1024 * 1024
PROVIDER_ENV_NAMES = frozenset(
    {
        "OPENAI_API_KEY",
        "OPENAI_BASE_URL",
        "ANTHROPIC_API_KEY",
        "GEMINI_API_KEY",
        "GROQ_API_KEY",
        "MOONSHOT_API_KEY",
        "DEEPSEEK_API_KEY",
        "XAI_API_KEY",
        "OPENROUTER_API_KEY",
        "AZURE_OPENAI_API_KEY",
        "AZURE_OPENAI_ENDPOINT",
    }
)
SAFE_PARENT_ENV_NAMES = frozenset(
    {
        "HOME",
        "PATH",
        "TMPDIR",
        "DYLD_LIBRARY_PATH",
        "LD_LIBRARY_PATH",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
    }
)


def free_loopback_port() -> int:
    with socket.socket() as candidate:
        candidate.bind(("127.0.0.1", 0))
        return int(candidate.getsockname()[1])


class ProfileLauncher:
    def __init__(self, root: Path, logs: Path, binary: Path, flows: Path) -> None:
        self.root = root
        self.logs = logs
        self.binary = binary
        self.flows = flows
        self.processes: dict[str, subprocess.Popen[bytes]] = {}
        self.handles: list[Any] = []

    def start(
        self, name: str, port: int, environment: dict[str, str]
    ) -> subprocess.Popen[bytes]:
        handle = (self.logs / f"replica-{name}.log").open("wb")
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
            stdout=handle,
            stderr=subprocess.STDOUT,
            start_new_session=os.name == "posix",
        )
        self.processes[name] = process
        self.handles.append(handle)
        return process

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
        outcomes = {}
        for name, process in self.processes.items():
            forced = False
            try:
                process.wait(timeout=max(0.0, deadline - time.monotonic()))
            except subprocess.TimeoutExpired:
                forced = True
                try:
                    if os.name == "posix":
                        os.killpg(process.pid, signal.SIGKILL)
                    else:
                        process.kill()
                except ProcessLookupError:
                    pass
                process.wait(timeout=5)
            outcomes[name] = {"exit_code": process.returncode, "forced_kill": forced}
        for handle in self.handles:
            handle.close()
        self.handles.clear()
        return outcomes


def child_environment(
    database_url: str,
    prefix: str,
    token: str,
    instance_id: str,
    provider_base_url: str,
    output_root: Path,
) -> dict[str, str]:
    """Build a minimal child environment after explicitly dropping paid-provider inputs."""
    inherited = os.environ.copy()
    for name in PROVIDER_ENV_NAMES:
        inherited.pop(name, None)
    environment = {
        name: inherited[name] for name in SAFE_PARENT_ENV_NAMES if name in inherited
    }
    environment.update(
        {
            "IRONCREW_STORE": "postgres",
            "DATABASE_URL": database_url,
            "IRONCREW_PG_TABLE_PREFIX": prefix,
            "IRONCREW_INSTANCE_ID": instance_id,
            "IRONCREW_API_TOKEN": token,
            "IRONCREW_API_PRINCIPAL": "ic018-profile",
            "IRONCREW_REQUIRE_IDEMPOTENCY_KEY": "true",
            "IRONCREW_DB_POOL_SIZE": "2",
            "IRONCREW_MAX_ACTIVE_RUNS": "2",
            "IRONCREW_MAX_ACTIVE_CONVERSATIONS": "2",
            "IRONCREW_MAX_SSE_CONNECTIONS": "4",
            "IRONCREW_MAX_EVENTS": "64",
            "IRONCREW_EVENT_JOURNAL_RETENTION_SECS": "120",
            "IRONCREW_EVENT_JOURNAL_PRUNE_BATCH": "64",
            "IRONCREW_ALLOW_PRIVATE_IPS": "true",
            "IRONCREW_ENV_ALLOWLIST": "IC018_PROFILE_PROVIDER_BASE_URL",
            "IC018_PROFILE_PROVIDER_BASE_URL": provider_base_url,
            "OPENAI_API_KEY": "ic018-loopback-not-a-secret",
            "OPENAI_BASE_URL": provider_base_url,
            "IRONCREW_FILE_WRITE_ROOT": str(output_root),
            "IRONCREW_MCP_ALLOWED_COMMANDS": "__disabled__",
            "IRONCREW_MCP_ALLOWED_HTTP_HOSTS": "__disabled__",
            "IRONCREW_LOG": "error",
        }
    )
    return environment


def scan_logs(logs: Path, canaries: tuple[str, ...]) -> dict[str, Any]:
    sizes = {}
    for path in sorted(logs.glob("replica-*.log")):
        content = path.read_bytes()
        if len(content) > MAX_LOG_BYTES:
            raise RuntimeError("profile runtime log exceeded one MiB")
        if any(canary and canary.encode() in content for canary in canaries):
            raise RuntimeError("profile runtime log failed the secret-canary check")
        sizes[path.name] = len(content)
    return {"files": sizes, "retained": False, "raw_content_in_report": False}
