"""Minimal fail-closed child environment for the provider-free soak."""

from __future__ import annotations

import os
from typing import Any


CHILD_ENV_ALLOWLIST = (
    "PATH",
    "TMPDIR",
    "TMP",
    "TEMP",
    "LANG",
    "LC_ALL",
    "TZ",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "SYSTEMROOT",
    "WINDIR",
)
PROVIDER_KEY_NAMES = (
    "OPENAI_API_KEY",
    "GEMINI_API_KEY",
    "GROQ_API_KEY",
    "MOONSHOT_API_KEY",
    "DEEPSEEK_API_KEY",
    "XAI_API_KEY",
    "OPENROUTER_API_KEY",
    "ANTHROPIC_API_KEY",
)


def child_environment(
    args: Any,
    instance_id: str,
    token: str,
    prefix: str,
    keyring_json: str,
) -> dict[str, str]:
    """Return only required system/runtime values; never inherit credentials."""
    environment = {
        name: os.environ[name] for name in CHILD_ENV_ALLOWLIST if name in os.environ
    }
    environment.update(
        {
            "IRONCREW_STORE": "postgres",
            "DATABASE_URL": args.database_url,
            "IRONCREW_PG_TABLE_PREFIX": prefix,
            "IRONCREW_INSTANCE_ID": instance_id,
            "IRONCREW_API_TOKEN": token,
            "IRONCREW_REQUIRE_IDEMPOTENCY_KEY": "true",
            "IRONCREW_HITL_ENCRYPTION_KEYS": keyring_json,
            "IRONCREW_HITL_ACTIVE_KEY_ID": "soak-v1",
            "IRONCREW_DB_POOL_SIZE": str(args.db_pool_size),
            "IRONCREW_MAX_ACTIVE_RUNS": str(args.max_active_runs),
            "IRONCREW_MAX_SSE_CONNECTIONS": str(max(4, args.max_active_runs * 2)),
            "IRONCREW_HITL_POLL_INTERVAL_MS": str(args.hitl_poll_ms),
            "IRONCREW_HITL_PG_MAX_CONCURRENT_READS": str(args.hitl_pg_reads),
            "IRONCREW_MAX_EVENTS": str(args.max_events),
            "IRONCREW_EVENT_REPLAY_MAX_BYTES": str(args.event_replay_max_bytes),
            "IRONCREW_EVENT_MAX_BYTES": str(args.event_max_bytes),
            "IRONCREW_EVENT_JOURNAL_RETENTION_SECS": str(
                args.journal_retention_seconds
            ),
            "IRONCREW_EVENT_JOURNAL_MAX_TOTAL_EVENTS": str(
                args.journal_max_total_events
            ),
            "IRONCREW_EVENT_JOURNAL_MAX_TOTAL_BYTES": str(
                args.journal_max_total_bytes
            ),
            "IRONCREW_EVENT_JOURNAL_PAGE_MAX_BYTES": str(
                args.journal_page_max_bytes
            ),
            "IRONCREW_EVENT_JOURNAL_POLL_INTERVAL_MS": str(args.journal_poll_ms),
            "IRONCREW_EVENT_JOURNAL_READ_TIMEOUT_MS": str(
                args.journal_read_timeout_ms
            ),
            "IRONCREW_EVENT_JOURNAL_WRITE_TIMEOUT_MS": str(
                args.journal_write_timeout_ms
            ),
            "IRONCREW_EVENT_JOURNAL_PRUNE_BATCH": str(args.journal_prune_batch),
            "IRONCREW_RUN_LEASE_TTL_SECONDS": "10",
            "IRONCREW_LOG": args.log_level,
            **{name: "" for name in PROVIDER_KEY_NAMES},
            "OPENAI_BASE_URL": "http://127.0.0.1:9/v1",
        }
    )
    return environment
