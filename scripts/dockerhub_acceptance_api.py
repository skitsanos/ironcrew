"""Bounded Docker Hub lifecycle calls for disposable IC-015 evidence."""

from __future__ import annotations

import hashlib
import json
import re
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime

from dockerhub_immutability import SEMVER_IMMUTABILITY_RULE

API_RESPONSE_LIMIT = 1024 * 1024
RUN_ID_RE = re.compile(r"^20[0-9]{6}t[0-9]{6}z-[0-9a-f]{8}$")


class AcceptanceError(RuntimeError):
    """The disposable-registry acceptance contract was not satisfied."""


def repository_name(run_id: str) -> str:
    if not RUN_ID_RE.fullmatch(run_id):
        raise AcceptanceError("run ID must match YYYYMMDDtHHMMSSz-xxxxxxxx")
    try:
        datetime.strptime(run_id[:16], "%Y%m%dt%H%M%Sz")
    except ValueError:
        raise AcceptanceError("run ID timestamp is invalid") from None
    return f"ironcrew-ic015-acceptance-{run_id}"


def description(run_id: str) -> str:
    return f"Disposable IronCrew IC-015 acceptance {run_id}"


class DockerHubApi:
    def __init__(self, *, namespace: str, run_id: str, username: str, secret: str,
                 api_base: str = "https://hub.docker.com"):
        if not re.fullmatch(r"[a-z0-9]+(?:[._-][a-z0-9]+)*", namespace):
            raise AcceptanceError("Docker Hub namespace is invalid")
        if not username or not secret:
            raise AcceptanceError("Docker Hub credentials are required")
        parsed = urllib.parse.urlsplit(api_base)
        loopback = parsed.hostname in {"127.0.0.1", "::1", "localhost"}
        if (parsed.scheme != "https" and not (parsed.scheme == "http" and loopback)) \
                or not parsed.netloc or parsed.query or parsed.fragment:
            raise AcceptanceError("Docker Hub API URL is invalid")
        self.namespace = namespace
        self.run_id = run_id
        self.name = repository_name(run_id)
        self.image = f"{namespace}/{self.name}"
        self.api_base = api_base.rstrip("/")
        auth = self._request(
            "POST", "/v2/auth/token", {"identifier": username, "secret": secret},
            expected={200}, authenticated=False,
        )
        token = auth.get("access_token") if isinstance(auth, dict) else None
        if not isinstance(token, str) or not token:
            raise AcceptanceError("Docker Hub authentication omitted a token")
        self._token = token

    @property
    def path(self) -> str:
        namespace = urllib.parse.quote(self.namespace, safe="")
        name = urllib.parse.quote(self.name, safe="")
        return f"/v2/namespaces/{namespace}/repositories/{name}"

    def _request(self, method: str, path: str, body: object | None = None, *,
                 expected: set[int], authenticated: bool = True) -> object:
        headers = {"Content-Type": "application/json"}
        if authenticated:
            headers["Authorization"] = f"Bearer {getattr(self, '_token', '')}"
        request = urllib.request.Request(
            f"{self.api_base}{path}",
            data=None if body is None else json.dumps(body).encode(),
            headers=headers,
            method=method,
        )
        try:
            with urllib.request.urlopen(request, timeout=20) as response:
                payload = response.read(API_RESPONSE_LIMIT + 1)
                status = response.status
        except urllib.error.HTTPError as error:
            status = error.code
            payload = error.read(API_RESPONSE_LIMIT + 1)
            error.close()
        except (OSError, urllib.error.URLError, TimeoutError):
            raise AcceptanceError("Docker Hub API request failed") from None
        if len(payload) > API_RESPONSE_LIMIT:
            raise AcceptanceError("Docker Hub API response exceeded its byte limit")
        if status not in expected:
            raise AcceptanceError(f"Docker Hub API returned HTTP {status}")
        if status == 404 or not payload:
            return None
        try:
            return json.loads(payload)
        except (json.JSONDecodeError, ValueError):
            raise AcceptanceError("Docker Hub API response was invalid") from None

    def repository(self, *, allow_missing: bool = False) -> dict[str, object] | None:
        result = self._request(
            "GET", self.path, expected={200, 404} if allow_missing else {200}
        )
        if result is None:
            return None
        if not isinstance(result, dict):
            raise AcceptanceError("Docker Hub repository metadata was invalid")
        return result

    def create(self) -> dict[str, object]:
        if self.repository(allow_missing=True) is not None:
            raise AcceptanceError("disposable repository already exists")
        result = self._request(
            "POST", f"/v2/namespaces/{urllib.parse.quote(self.namespace, safe='')}/repositories",
            {
                "name": self.name,
                "namespace": self.namespace,
                "description": description(self.run_id),
                "registry": "docker.io",
                "is_private": False,
            },
            expected={201},
        )
        if not isinstance(result, dict):
            raise AcceptanceError("Docker Hub create response was invalid")
        self._request(
            "PATCH", f"{self.path}/immutabletags",
            {
                "immutable_tags": True,
                "immutable_tags_rules": [SEMVER_IMMUTABILITY_RULE],
            },
            expected={200},
        )
        return self.require_identity()

    def require_identity(self) -> dict[str, object]:
        state = self.repository()
        assert state is not None
        expected = {
            "namespace": self.namespace,
            "name": self.name,
            "description": description(self.run_id),
            "repository_type": "image",
            "is_private": False,
        }
        if any(state.get(key) != value for key, value in expected.items()):
            raise AcceptanceError("Docker Hub repository identity did not match the run")
        permissions = state.get("permissions")
        if not isinstance(permissions, dict) or permissions.get("admin") is not True:
            raise AcceptanceError("Docker Hub repository admin permission was not proven")
        settings = state.get("immutable_tags_settings")
        if settings != {
            "enabled": True,
            "rules": [SEMVER_IMMUTABILITY_RULE],
        }:
            raise AcceptanceError("Docker Hub immutable-tag policy did not match")
        registered = state.get("date_registered")
        if not isinstance(registered, str) or not registered:
            raise AcceptanceError("Docker Hub repository registration identity was absent")
        return state

    def tags(self) -> list[str]:
        result = self._request(
            "GET", f"{self.path}/tags?page=1&page_size=100", expected={200}
        )
        if not isinstance(result, dict) or not isinstance(result.get("results"), list):
            raise AcceptanceError("Docker Hub tag inventory was invalid")
        rows = result["results"]
        names = [row.get("name") for row in rows if isinstance(row, dict)]
        count = result.get("count")
        if not isinstance(count, int) or count not in {0, len(rows)} \
                or result.get("next") is not None or result.get("previous") is not None \
                or len(rows) > 100 \
                or len(names) != len(rows) \
                or any(not isinstance(name, str) for name in names) \
                or len(set(names)) != len(names):
            raise AcceptanceError("Docker Hub tag inventory was incomplete")
        return sorted(names)  # type: ignore[arg-type]


def fingerprint(state: dict[str, object]) -> str:
    fields = {key: state.get(key) for key in (
        "namespace", "name", "description", "date_registered", "repository_type"
    )}
    return hashlib.sha256(json.dumps(fields, sort_keys=True).encode()).hexdigest()
