"""Bounded HTTP transport for IC-008 platform conversation probes."""

from __future__ import annotations

import http.client
import json
import urllib.error
import urllib.parse
import urllib.request
from collections.abc import Mapping

from conversation_receipt import sanitize_record


MAX_REQUEST_BYTES = 256 * 1024
MAX_RESPONSE_BYTES = 1024 * 1024
ABSENT = object()


class ConversationContractError(RuntimeError):
    """A fixed-message conversation probe failure."""


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *_args: object, **_kwargs: object) -> None:
        return None


def _base_url(value: str) -> str:
    parsed = urllib.parse.urlsplit(value)
    if (
        parsed.scheme not in {"http", "https"}
        or not parsed.hostname
        or parsed.username is not None
        or parsed.password is not None
        or parsed.query
        or parsed.fragment
    ):
        raise ConversationContractError("base URL is invalid")
    return value.rstrip("/")


def component(value: str, label: str) -> str:
    if (
        not 1 <= len(value.encode("utf-8")) <= 128
        or not value.isascii()
        or not all(character.isalnum() or character in "._-" for character in value)
    ):
        raise ConversationContractError(f"{label} is invalid")
    return urllib.parse.quote(value, safe="")


class ConversationHttpClient:
    """Make bounded requests and return only receipt-safe projections."""

    def __init__(
        self,
        base_url: str,
        bearer_token: str,
        *,
        mock_base_url: str | None = None,
        timeout_seconds: float = 10,
    ) -> None:
        if not bearer_token or any(character.isspace() for character in bearer_token):
            raise ConversationContractError("bearer token is invalid")
        if not 0 < timeout_seconds <= 60:
            raise ConversationContractError("request timeout is invalid")
        self._base = _base_url(base_url)
        self._mock_base = _base_url(mock_base_url) if mock_base_url else None
        self._token = bearer_token
        self._timeout = timeout_seconds
        self._opener = urllib.request.build_opener(_NoRedirect)

    def request(
        self,
        operation: str,
        method: str,
        path: str,
        *,
        payload: object = ABSENT,
        idempotency_key: str | None = None,
        extra_headers: Mapping[str, str] | None = None,
        mock: bool = False,
        secret_values: tuple[str, ...] = (),
    ) -> dict[str, object]:
        encoded = None
        headers = {"Accept": "application/json"}
        secrets = (self._token, *secret_values)
        if not mock:
            headers["Authorization"] = f"Bearer {self._token}"
        if payload is not ABSENT:
            try:
                encoded = json.dumps(
                    payload, sort_keys=True, separators=(",", ":"), allow_nan=False
                ).encode()
            except (TypeError, ValueError):
                raise ConversationContractError("request body is not canonical JSON") from None
            if len(encoded) > MAX_REQUEST_BYTES:
                raise ConversationContractError("request body exceeds the byte limit")
            headers["Content-Type"] = "application/json"
        if idempotency_key is not None:
            if not 1 <= len(idempotency_key) <= 128 or any(
                ord(character) < 33 or ord(character) > 126 for character in idempotency_key
            ):
                raise ConversationContractError("idempotency key is invalid")
            headers["Idempotency-Key"] = idempotency_key
            secrets = (*secrets, idempotency_key)
        headers.update(extra_headers or {})
        root = self._mock_base if mock else self._base
        if root is None:
            raise ConversationContractError("mock base URL is not configured")
        request = urllib.request.Request(root + path, data=encoded, headers=headers, method=method)
        try:
            response = self._opener.open(request, timeout=self._timeout)
        except urllib.error.HTTPError as error:
            response = error
        except (OSError, urllib.error.URLError):
            raise ConversationContractError("HTTP request failed") from None
        try:
            try:
                raw = response.read(MAX_RESPONSE_BYTES + 1)
            except (OSError, http.client.HTTPException):
                raise ConversationContractError("HTTP response read failed") from None
            if len(raw) > MAX_RESPONSE_BYTES:
                raise ConversationContractError("HTTP response exceeds the byte limit")
            try:
                parsed = json.loads(raw) if raw else None
            except (ValueError, UnicodeDecodeError, RecursionError):
                raise ConversationContractError("HTTP response is not valid JSON") from None
            captured = {
                name: value
                for name in (
                    "cache-control",
                    "content-type",
                    "idempotency-replayed",
                    "x-accel-buffering",
                    "x-ironcrew-instance-id",
                )
                if (value := response.headers.get(name)) is not None
            }
            return sanitize_record(
                {
                    "operation": operation,
                    "status": response.status,
                    "receiver": captured.get("x-ironcrew-instance-id"),
                    "headers": captured,
                    "response": parsed,
                    "response_bytes": len(raw),
                },
                secrets,
            )
        finally:
            response.close()
