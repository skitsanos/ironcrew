"""Strict, bounded receipt projection for IC-008 platform probes."""

from __future__ import annotations

import math
import re
from collections.abc import Iterable, Mapping
from itertools import islice


SCHEMA = "ironcrew-ic008-platform-canary-v1"
MAX_RECORDS = 256
MAX_LIST_ITEMS = 128
MAX_DEPTH = 5
MAX_STRING_BYTES = 512
OPERATION = re.compile(r"[a-z][a-z0-9_]{0,63}\Z")
SAFE_HEADERS = frozenset(
    {
        "cache-control",
        "content-type",
        "idempotency-replayed",
        "x-accel-buffering",
        "x-ironcrew-instance-id",
    }
)
SAFE_RESPONSE_FIELDS = frozenset(
    "agent artifact_fingerprint blocked blocked_requests blocking_content_configured "
    "chat_completions code config_fingerprint conversation_id deleted definition_fingerprint "
    "deployment effect_calls events_url final_responses flow flow_fingerprint "
    "hitl_keyring_fingerprint incarnation_id instance_id lifecycle_state messages "
    "process_start_id release_generation released_requests revision role source_fingerprint "
    "status tool_call_responses truncated turn_count turn_index".split()
)


class ReceiptError(RuntimeError):
    """A fixed-message receipt validation failure."""


def _secrets(values: Iterable[str]) -> tuple[str, ...]:
    result = tuple(
        value for value in islice(values, 65) if isinstance(value, str) and value
    )
    if len(result) > 64 or any(len(value.encode("utf-8")) > 4096 for value in result):
        raise ReceiptError("receipt secret set exceeds its bound")
    return result


def _string(value: object, secrets: tuple[str, ...]) -> str | None:
    if not isinstance(value, str):
        return None
    if any(secret in value for secret in secrets):
        return "<redacted>"
    if not value.isprintable():
        return "<invalid>"
    encoded = value.encode("utf-8")
    if len(encoded) <= MAX_STRING_BYTES:
        return value
    return encoded[:MAX_STRING_BYTES].decode("utf-8", errors="ignore")


def _response(value: object, secrets: tuple[str, ...], depth: int = 0) -> object:
    if depth > MAX_DEPTH:
        return None
    if isinstance(value, Mapping):
        return {
            key: _response(item, secrets, depth + 1)
            for key, item in islice(value.items(), MAX_LIST_ITEMS)
            if isinstance(key, str) and key in SAFE_RESPONSE_FIELDS
        }
    if isinstance(value, list):
        return [_response(item, secrets, depth + 1) for item in value[:MAX_LIST_ITEMS]]
    safe_string = _string(value, secrets)
    if safe_string is not None:
        return safe_string
    if value is None or isinstance(value, bool):
        return value
    if isinstance(value, int):
        return value
    if isinstance(value, float):
        return value if math.isfinite(value) else None
    return None


def sanitize_record(record: Mapping[str, object], secrets: Iterable[str] = ()) -> dict[str, object]:
    """Return only non-secret fields suitable for a durable canary receipt."""
    secret_values = _secrets(secrets)
    operation = record.get("operation")
    status = record.get("status")
    response_bytes = record.get("response_bytes")
    if not isinstance(operation, str) or not OPERATION.fullmatch(operation):
        raise ReceiptError("receipt operation is invalid")
    if type(status) is not int or not 100 <= status <= 599:
        raise ReceiptError("receipt status is invalid")
    if type(response_bytes) is not int or not 0 <= response_bytes <= 16 * 1024 * 1024:
        raise ReceiptError("receipt response size is invalid")
    result: dict[str, object] = {
        "operation": operation,
        "status": status,
        "response_bytes": response_bytes,
    }
    receiver = _string(record.get("receiver"), secret_values)
    if receiver is not None:
        result["receiver"] = receiver
    headers = record.get("headers")
    if isinstance(headers, Mapping):
        result["headers"] = {
            name.lower(): safe
            for name, value in islice(headers.items(), 32)
            if isinstance(name, str)
            and name.lower() in SAFE_HEADERS
            and (safe := _string(value, secret_values)) is not None
        }
    response = record.get("response")
    if response is not None:
        result["response"] = _response(response, secret_values)
    return result


def sanitize_receipt(
    records: Iterable[Mapping[str, object]], secrets: Iterable[str] = ()
) -> dict[str, object]:
    """Build one bounded receipt without request bodies, credentials, or URLs."""
    items = list(islice(records, MAX_RECORDS + 1))
    if not 1 <= len(items) <= MAX_RECORDS:
        raise ReceiptError("receipt record count is invalid")
    secret_values = _secrets(secrets)
    return {
        "schema": SCHEMA,
        "record_count": len(items),
        "records": [sanitize_record(record, secret_values) for record in items],
    }
