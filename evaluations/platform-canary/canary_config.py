#!/usr/bin/env python3
"""Emit the complete, non-secret IC-007 canary environment as JSON."""

from __future__ import annotations

import argparse
import ipaddress
import json
import re
from urllib.parse import urlsplit

from config_contract import CONFIG_ENV_ALLOWLIST


MAX_TABLE_PREFIX_BYTES = 37
MAX_PROVIDER_BASE_URL_BYTES = 2048
TABLE_PREFIX = re.compile(r"[a-z0-9][a-z0-9_]{0,35}_\Z")
DNS_LABEL = re.compile(r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\Z")
SUBSTITUTION_NAMES = {
    "IRONCREW_PG_TABLE_PREFIX",
    "PLATFORM_CANARY_PROVIDER_BASE_URL",
}


class CanaryConfigError(ValueError):
    """A fixed-message canary-configuration validation failure."""


# Keep every fixed value visible and reviewable. The two deployment-specific
# substitutions are inserted only after validation in canary_environment().
FIXED_CONFIG_ITEMS = (
    ("IRONCREW_STORE", "postgres"),
    ("IRONCREW_REQUIRE_IDEMPOTENCY_KEY", "true"),
    ("IRONCREW_IDEMPOTENCY_TTL_SECONDS", "7200"),
    ("IRONCREW_IDEMPOTENCY_MAX_RECORDS", "1000"),
    ("IRONCREW_IDEMPOTENCY_MAX_RESPONSE_BYTES", "1048576"),
    ("IRONCREW_IDEMPOTENCY_MAX_TOTAL_RESPONSE_BYTES", "16777216"),
    ("IRONCREW_IDEMPOTENCY_MAX_RECORDS_PER_PRINCIPAL", "250"),
    ("IRONCREW_IDEMPOTENCY_MAX_IN_FLIGHT_PER_PRINCIPAL", "8"),
    ("IRONCREW_IDEMPOTENCY_MAX_TOTAL_RESPONSE_BYTES_PER_PRINCIPAL", "4194304"),
    ("IRONCREW_IDEMPOTENCY_PRUNE_BATCH", "100"),
    ("IRONCREW_DB_POOL_SIZE", "2"),
    ("IRONCREW_DB_CONNECT_RETRIES", "10"),
    ("IRONCREW_DB_CONNECT_BACKOFF_MS", "1000"),
    ("IRONCREW_DB_CONNECT_TIMEOUT_SECS", "30"),
    ("IRONCREW_API_PRINCIPAL", "platform-canary"),
    ("IRONCREW_ALLOW_UNAUTHENTICATED", "false"),
    ("IRONCREW_TRUST_PROXY", "false"),
    ("IRONCREW_LOG", "info"),
    ("IRONCREW_MAX_BODY_SIZE", "10485760"),
    ("IRONCREW_MAX_ACTIVE_RUNS", "2"),
    ("IRONCREW_MAX_ACTIVE_CONVERSATIONS", "2"),
    ("IRONCREW_MAX_CONVERSATION_LIFECYCLES", "32"),
    ("IRONCREW_MAX_SSE_CONNECTIONS", "8"),
    ("IRONCREW_MAX_EVENTS", "256"),
    ("IRONCREW_EVENT_MAX_BYTES", "65536"),
    ("IRONCREW_EVENT_REPLAY_MAX_BYTES", "1048576"),
    ("IRONCREW_EVENT_CHANNEL_CAPACITY", "8"),
    ("IRONCREW_EVENT_JOURNAL_RETENTION_SECS", "600"),
    ("IRONCREW_EVENT_JOURNAL_MAX_TOTAL_EVENTS", "2000"),
    ("IRONCREW_EVENT_JOURNAL_MAX_TOTAL_BYTES", "16777216"),
    ("IRONCREW_EVENT_JOURNAL_PAGE_MAX_BYTES", "131072"),
    ("IRONCREW_EVENT_JOURNAL_POLL_INTERVAL_MS", "250"),
    ("IRONCREW_EVENT_JOURNAL_READ_TIMEOUT_MS", "2000"),
    ("IRONCREW_EVENT_JOURNAL_WRITE_TIMEOUT_MS", "5000"),
    ("IRONCREW_EVENT_JOURNAL_PRUNE_BATCH", "100"),
    # Use the documented production default for platform rollouts. Short
    # leases shrink the maintenance watchdog as well as owner-loss latency;
    # they are useful for isolated fault injection but can make a concurrent
    # rolling bootstrap contend on normal schema and reconciliation work.
    ("IRONCREW_RUN_LEASE_TTL_SECONDS", "60"),
    ("IRONCREW_RUN_SSE_RETENTION_SECS", "5"),
    ("IRONCREW_MAX_RUN_LIFETIME", "300"),
    ("IRONCREW_ASK_HUMAN_TIMEOUT", "120"),
    ("IRONCREW_ASK_HUMAN_MAX_TIMEOUT", "300"),
    ("IRONCREW_ASK_HUMAN_MAX_PENDING", "8"),
    ("IRONCREW_ASK_HUMAN_MAX_PENDING_BYTES", "262144"),
    ("IRONCREW_ASK_HUMAN_MAX_PROMPT_BYTES", "16384"),
    ("IRONCREW_ASK_HUMAN_MAX_CHOICES", "16"),
    ("IRONCREW_ASK_HUMAN_MAX_CHOICES_BYTES", "16384"),
    ("IRONCREW_ASK_HUMAN_MAX_ANSWER_BYTES", "16384"),
    ("IRONCREW_HITL_POLL_INTERVAL_MS", "250"),
    ("IRONCREW_HITL_READ_TIMEOUT_MS", "2000"),
    ("IRONCREW_HITL_PG_MAX_CONCURRENT_READS", "1"),
    ("IRONCREW_ADMISSION_WORK_RATE_PER_MINUTE", "60"),
    ("IRONCREW_ADMISSION_WORK_BURST", "10"),
    ("IRONCREW_ADMISSION_CONTROL_RATE_PER_MINUTE", "120"),
    ("IRONCREW_ADMISSION_CONTROL_BURST", "20"),
    ("IRONCREW_ADMISSION_OBSERVATION_RATE_PER_MINUTE", "600"),
    ("IRONCREW_ADMISSION_OBSERVATION_BURST", "20"),
    ("IRONCREW_READINESS_CACHE_MS", "500"),
    ("IRONCREW_SHUTDOWN_ROUTING_GRACE_SECS", "5"),
    ("IRONCREW_SHUTDOWN_TIMEOUT_SECS", "15"),
    ("IRONCREW_SHUTDOWN_DRAIN_MS", "1000"),
    ("IRONCREW_LUA_MAX_MEMORY_BYTES", "16777216"),
    ("IRONCREW_LUA_MAX_EXECUTION_SECONDS", "60"),
    ("IRONCREW_LUA_MAX_INSTRUCTIONS", "10000000"),
    ("IRONCREW_LUA_MAX_SOURCE_BYTES", "1048576"),
    ("IRONCREW_LUA_JSON_MAX_DEPTH", "64"),
    ("IRONCREW_LUA_JSON_MAX_NODES", "100000"),
    ("IRONCREW_LUA_JSON_MAX_STRING_BYTES", "8388608"),
    ("IRONCREW_LUA_JSON_MAX_OUTPUT_BYTES", "16777216"),
    ("IRONCREW_LUA_FS_MAX_READ_BYTES", "1048576"),
    ("IRONCREW_LUA_FS_MAX_WRITE_BYTES", "1048576"),
    ("IRONCREW_MAX_AGENTS", "64"),
    ("IRONCREW_CREW_GOAL_MAX_BYTES", "65536"),
    ("IRONCREW_MAX_PROMPT_CHARS", "16384"),
    ("IRONCREW_MAX_TASKS", "8"),
    ("IRONCREW_MAX_CONCURRENT_TASKS", "2"),
    ("IRONCREW_MAX_TASK_RETRIES", "10"),
    ("IRONCREW_MAX_TASK_TIMEOUT_SECS", "86400"),
    ("IRONCREW_TOOL_TIMEOUT", "60"),
    ("IRONCREW_TASK_RESULT_MAX_OUTPUT_BYTES", "1048576"),
    ("IRONCREW_TASK_RESULT_MAX_REASONING_BYTES", "524288"),
    ("IRONCREW_RUN_RESULTS_MAX_BYTES", "4194304"),
    ("IRONCREW_PROVIDER_MAX_RESPONSE_BYTES", "1048576"),
    ("IRONCREW_PROVIDER_MAX_ERROR_BYTES", "65536"),
    ("IRONCREW_PROVIDER_MAX_STREAM_BYTES", "2097152"),
    ("IRONCREW_PROVIDER_MAX_OUTPUT_BYTES", "1048576"),
    ("IRONCREW_CHAT_SESSION_IDLE_SECS", "1800"),
    ("IRONCREW_MAX_CONVERSATION_TURN_SECS", "300"),
    ("IRONCREW_API_CONVERSATION_MAX_HISTORY", "50"),
    ("IRONCREW_API_MESSAGE_MAX_BYTES", "262144"),
    ("IRONCREW_CONVERSATION_MAX_HISTORY", "50"),
    ("IRONCREW_CHAT_HISTORY_MAX_BYTES", "33554432"),
    ("IRONCREW_MAX_REASONING_BYTES", "1048576"),
    ("IRONCREW_HTTP_MAX_REQUEST_HEADER_BYTES", "32768"),
    ("IRONCREW_HTTP_MAX_REQUEST_BODY_BYTES", "262144"),
    ("IRONCREW_HTTP_MAX_RESPONSE_BYTES", "262144"),
    ("IRONCREW_HTTP_MAX_HEADER_BYTES", "32768"),
    ("IRONCREW_HTTP_MAX_JSON_BYTES", "262144"),
    ("IRONCREW_HTTP_MAX_OUTPUT_BYTES", "524288"),
    ("IRONCREW_ALLOW_PRIVATE_IPS", "true"),
    ("IRONCREW_ENV_ALLOWLIST", "PLATFORM_CANARY_PROVIDER_BASE_URL"),
    ("IRONCREW_MCP_ALLOWED_COMMANDS", "__disabled__"),
    ("IRONCREW_MCP_ALLOWED_HTTP_HOSTS", "__disabled__"),
    ("IRONCREW_MCP_ALLOW_LOCALHOST", "false"),
    ("IRONCREW_MCP_TOOL_RESULT_MAX_BYTES", "262144"),
    ("IRONCREW_FILE_WRITE_ROOT", "/data/outputs"),
    ("IRONCREW_FILE_WRITE_MAX_BYTES", "262144"),
)


def _validated_table_prefix(value: str) -> str:
    if (
        not isinstance(value, str)
        or not value.isascii()
        or len(value) > MAX_TABLE_PREFIX_BYTES
        or TABLE_PREFIX.fullmatch(value) is None
    ):
        raise CanaryConfigError(
            "table prefix must be 2-37 lowercase ASCII alphanumeric/underscore "
            "bytes and end with underscore"
        )
    return value


def _valid_provider_hostname(hostname: str) -> bool:
    try:
        ipaddress.ip_address(hostname)
        return True
    except ValueError:
        labels = hostname.split(".")
        return len(hostname) <= 253 and all(DNS_LABEL.fullmatch(label) for label in labels)


def _validated_provider_base_url(value: str) -> str:
    if not isinstance(value, str):
        raise CanaryConfigError("provider base URL must be a canonical HTTP(S) /v1 URL")
    try:
        encoded = value.encode("ascii")
        parsed = urlsplit(value)
        port = parsed.port
    except (UnicodeEncodeError, ValueError) as error:
        raise CanaryConfigError(
            "provider base URL must be a canonical HTTP(S) /v1 URL"
        ) from error
    if (
        not encoded
        or len(encoded) > MAX_PROVIDER_BASE_URL_BYTES
        or parsed.scheme not in {"http", "https"}
        or parsed.hostname is None
        or not _valid_provider_hostname(parsed.hostname)
        or parsed.username is not None
        or parsed.password is not None
        or parsed.path != "/v1"
        or parsed.query
        or parsed.fragment
        or parsed.geturl() != value
        or any(byte < 0x21 or byte > 0x7E for byte in encoded)
        or port == 0
        or (parsed.netloc.endswith(":") and port is None)
    ):
        raise CanaryConfigError("provider base URL must be a canonical HTTP(S) /v1 URL")
    return value


def canary_environment(table_prefix: str, provider_base_url: str) -> dict[str, str]:
    """Return one validated value for every effective-config allowlist name."""
    items = FIXED_CONFIG_ITEMS + (
        ("IRONCREW_PG_TABLE_PREFIX", _validated_table_prefix(table_prefix)),
        (
            "PLATFORM_CANARY_PROVIDER_BASE_URL",
            _validated_provider_base_url(provider_base_url),
        ),
    )
    names = tuple(name for name, _value in items)
    if len(set(names)) != len(names):
        raise CanaryConfigError("canary configuration contains duplicate names")
    expected = set(CONFIG_ENV_ALLOWLIST)
    if set(names) != expected or len(names) != len(CONFIG_ENV_ALLOWLIST):
        raise CanaryConfigError("canary configuration does not match the effective-config contract")
    values = dict(items)
    return {name: values[name] for name in CONFIG_ENV_ALLOWLIST}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--table-prefix", required=True)
    parser.add_argument("--provider-base-url", required=True)
    args = parser.parse_args()
    try:
        environment = canary_environment(args.table_prefix, args.provider_base_url)
    except CanaryConfigError as error:
        parser.error(str(error))
    print(json.dumps(environment, sort_keys=True, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
