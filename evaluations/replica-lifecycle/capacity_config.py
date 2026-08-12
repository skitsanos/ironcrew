"""Frozen limits and process configuration for the IC-020 local gate."""

from __future__ import annotations

import hashlib
import os
import subprocess
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
HERE = Path(__file__).resolve().parent
REPLICA_COUNTS = (1, 2, 3)
DB_POOL_PER_REPLICA = 2
ACTIVE_RUNS_PER_REPLICA = 2
SSE_PER_REPLICA = 2
RSS_PER_PROCESS_BYTES = 256 * 1024 * 1024
MAX_EVENTS_PER_RUN = 32
REPLAY_BYTES_PER_RUN = 256 * 1024
EVENT_MAX_BYTES = 32 * 1024
JOURNAL_MAX_TOTAL_EVENTS = 256
JOURNAL_MAX_TOTAL_BYTES = 4 * 1024 * 1024
DURABLE_QUEUE_BYTES_PER_RUN = REPLAY_BYTES_PER_RUN
EVENT_PAYLOAD_ENVELOPE_PER_PROCESS = ACTIVE_RUNS_PER_REPLICA * (
    REPLAY_BYTES_PER_RUN + DURABLE_QUEUE_BYTES_PER_RUN
)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def git_revision() -> tuple[str, bool]:
    commit = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=ROOT, check=True, capture_output=True, text=True
    ).stdout.strip()
    dirty = subprocess.run(["git", "diff", "--quiet", "HEAD", "--"], cwd=ROOT).returncode != 0
    untracked = subprocess.run(
        ["git", "ls-files", "--others", "--exclude-standard"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    return commit, dirty or bool(untracked)


def child_environment(
    database_url: str, prefix: str, instance_id: str, token: str, provider_url: str
) -> dict[str, str]:
    environment = os.environ.copy()
    for name in ("PORT", "IRONCREW_PORT", "IRONCREW_HOST"):
        environment.pop(name, None)
    environment.update(
        {
            "IRONCREW_STORE": "postgres",
            "DATABASE_URL": database_url,
            "IRONCREW_PG_TABLE_PREFIX": prefix,
            "IRONCREW_INSTANCE_ID": instance_id,
            "IRONCREW_API_TOKEN": token,
            "IRONCREW_REQUIRE_IDEMPOTENCY_KEY": "true",
            "IRONCREW_DB_POOL_SIZE": str(DB_POOL_PER_REPLICA),
            "IRONCREW_MAX_ACTIVE_RUNS": str(ACTIVE_RUNS_PER_REPLICA),
            "IRONCREW_MAX_ACTIVE_CONVERSATIONS": "1",
            "IRONCREW_MAX_SSE_CONNECTIONS": str(SSE_PER_REPLICA),
            "IRONCREW_MAX_EVENTS": str(MAX_EVENTS_PER_RUN),
            "IRONCREW_EVENT_REPLAY_MAX_BYTES": str(REPLAY_BYTES_PER_RUN),
            "IRONCREW_EVENT_MAX_BYTES": str(EVENT_MAX_BYTES),
            "IRONCREW_EVENT_CHANNEL_CAPACITY": "8",
            "IRONCREW_EVENT_JOURNAL_MAX_TOTAL_EVENTS": str(JOURNAL_MAX_TOTAL_EVENTS),
            "IRONCREW_EVENT_JOURNAL_MAX_TOTAL_BYTES": str(JOURNAL_MAX_TOTAL_BYTES),
            "IRONCREW_EVENT_JOURNAL_PAGE_MAX_BYTES": str(64 * 1024),
            "IRONCREW_EVENT_JOURNAL_POLL_INTERVAL_MS": "100",
            "IRONCREW_EVENT_JOURNAL_READ_TIMEOUT_MS": "2000",
            "IRONCREW_EVENT_JOURNAL_PRUNE_BATCH": "128",
            "IRONCREW_RUN_LEASE_TTL_SECONDS": "10",
            "IRONCREW_RUN_SSE_RETENTION_SECS": "1",
            "IRONCREW_LUA_MAX_MEMORY_BYTES": str(16 * 1024 * 1024),
            "IRONCREW_ALLOW_PRIVATE_IPS": "true",
            "IRONCREW_ENV_ALLOWLIST": "IC020_PROVIDER_BASE_URL",
            "IRONCREW_SHUTDOWN_ROUTING_GRACE_SECS": "0",
            "IRONCREW_SHUTDOWN_TIMEOUT_SECS": "8",
            "IRONCREW_SHUTDOWN_DRAIN_MS": "0",
            "IRONCREW_LOG": "warn",
            "OPENAI_API_KEY": "ic020-loopback-not-a-secret",
            "OPENAI_BASE_URL": provider_url,
            "IC020_PROVIDER_BASE_URL": provider_url,
        }
    )
    return environment


def container_contract(container: str) -> str:
    result = subprocess.run(
        ["docker", "inspect", "--format", "{{.State.Running}}|{{.Config.Image}}", container],
        check=True,
        capture_output=True,
        text=True,
        timeout=10,
    ).stdout.strip()
    if result != "true|postgres:15":
        raise RuntimeError(f"expected a running postgres:15 container, observed {result!r}")
    return "postgres:15"
