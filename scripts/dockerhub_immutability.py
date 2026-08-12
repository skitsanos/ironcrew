#!/usr/bin/env python3
"""Fail-closed verification of Docker Hub semver tag immutability."""

from __future__ import annotations

import json
import re
import urllib.error
import urllib.parse
import urllib.request
from typing import Any

DOCKER_HUB_API = "https://hub.docker.com"
MAX_API_RESPONSE_BYTES = 1024 * 1024
SEMVER_IMMUTABILITY_RULE = (
    r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$"
)
_IMAGE_RE = re.compile(r"^[a-z0-9]+(?:[._-][a-z0-9]+)*/[a-z0-9]+(?:[._-][a-z0-9]+)*$")


class ImmutabilityPolicyError(RuntimeError):
    """The registry did not prove the required immutable-tag policy."""


def _request_json(request: urllib.request.Request, *, timeout: float = 20.0) -> Any:
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            if response.status != 200:
                raise ImmutabilityPolicyError(
                    f"Docker Hub API returned HTTP {response.status}"
                )
            body = response.read(MAX_API_RESPONSE_BYTES + 1)
            if len(body) > MAX_API_RESPONSE_BYTES:
                raise ImmutabilityPolicyError("Docker Hub API response exceeded its byte limit")
            return json.loads(body)
    except urllib.error.HTTPError as error:
        error.close()
        raise ImmutabilityPolicyError(
            f"Docker Hub API returned HTTP {error.code}"
        ) from None
    except (OSError, urllib.error.URLError, TimeoutError, json.JSONDecodeError, ValueError):
        raise ImmutabilityPolicyError("Docker Hub API response was unavailable or invalid") from None


def _validate_api_base(api_base: str) -> str:
    parsed = urllib.parse.urlsplit(api_base)
    loopback = parsed.hostname in {"127.0.0.1", "::1", "localhost"}
    if parsed.scheme != "https" and not (parsed.scheme == "http" and loopback):
        raise ImmutabilityPolicyError("Docker Hub API URL must use HTTPS")
    if not parsed.netloc or parsed.query or parsed.fragment:
        raise ImmutabilityPolicyError("Docker Hub API URL is invalid")
    return api_base.rstrip("/")


def _split_image(image: str) -> tuple[str, str]:
    if not _IMAGE_RE.fullmatch(image):
        raise ImmutabilityPolicyError("image must be a lowercase namespace/repository")
    return tuple(image.split("/", 1))  # type: ignore[return-value]


def require_semver_immutability(
    *,
    image: str,
    username: str,
    secret: str,
    api_base: str = DOCKER_HUB_API,
) -> None:
    """Authenticate and require Docker Hub's canonical semver-only lock rule."""
    if not username or not secret:
        raise ImmutabilityPolicyError("Docker Hub credentials are required")
    namespace, repository = _split_image(image)
    base = _validate_api_base(api_base)
    auth_request = urllib.request.Request(
        f"{base}/v2/auth/token",
        data=json.dumps({"identifier": username, "secret": secret}).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    auth = _request_json(auth_request)
    token = auth.get("access_token") if isinstance(auth, dict) else None
    if not isinstance(token, str) or not token:
        raise ImmutabilityPolicyError("Docker Hub authentication response omitted a token")

    policy_request = urllib.request.Request(
        f"{base}/v2/namespaces/{urllib.parse.quote(namespace)}/repositories/"
        f"{urllib.parse.quote(repository)}",
        headers={"Authorization": f"Bearer {token}"},
    )
    repository_state = _request_json(policy_request)
    settings = (
        repository_state.get("immutable_tags_settings")
        if isinstance(repository_state, dict)
        else None
    )
    if not isinstance(settings, dict) or settings.get("enabled") is not True:
        raise ImmutabilityPolicyError("Docker Hub immutable tags are not enabled")
    rules = settings.get("rules")
    if not isinstance(rules, list) or any(not isinstance(rule, str) for rule in rules):
        raise ImmutabilityPolicyError("Docker Hub immutable-tag rules are invalid")
    if rules != [SEMVER_IMMUTABILITY_RULE]:
        raise ImmutabilityPolicyError(
            "Docker Hub must enforce only the required canonical semver rule"
        )
