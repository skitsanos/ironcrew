"""Bounded, secret-safe HTTP contract probes for platform canaries."""
from __future__ import annotations
import http.client
import json, re, socket, time
import urllib.error, urllib.parse, urllib.request
from typing import Any
MAX_REQUEST_BYTES = 256 * 1024
MAX_JSON_BYTES = 1024 * 1024
MAX_SSE_BYTES = 2 * 1024 * 1024
MAX_SSE_LINE_BYTES = 64 * 1024
MAX_SSE_EVENTS = 256
MAX_CAPABILITY_SAMPLES = 64
MAX_POLL_ATTEMPTS = 120
SAFE_HEADERS = (
    "cache-control",
    "content-type",
    "idempotency-replayed",
    "retry-after",
    "x-accel-buffering",
    "x-ironcrew-instance-id",
)
SAFE_FIELDS = frozenset(
    "already_requested artifact_fingerprint chat_completions code config_fingerprint "
    "control_scope cross_instance deployment effect_calls events_url final_responses "
    "flow_fingerprint hitl_keyring_fingerprint human_input instance_id journal_complete "
    "lifecycle_state live_control multi_replica_control owner_instance_id process_start_id "
    "question_id questions retryable revision run_abort run_id sse_replay status "
    "synthesized_from_run_record tool_call_responses topology".split()
)
COMPONENT = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}\Z")
_ABSENT = object()
class ContractError(RuntimeError):
    """A fixed-message contract probe failure."""
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
        raise ContractError("base URL is invalid")
    return value.rstrip("/")
def _component(value: str, label: str) -> str:
    if not COMPONENT.fullmatch(value):
        raise ContractError(f"{label} is invalid")
    return urllib.parse.quote(value, safe="")
def _secret_strings(value: object) -> set[str]:
    if isinstance(value, str):
        return {value} if value else set()
    if isinstance(value, list):
        return set().union(*(_secret_strings(item) for item in value), set())
    if isinstance(value, dict):
        return set().union(*(_secret_strings(item) for item in value.values()), set())
    return set()
def _safe_json(value: object, secrets: set[str], depth: int = 0) -> object:
    if depth > 5:
        return None
    if isinstance(value, dict):
        return {
            key: _safe_json(item, secrets, depth + 1)
            for key, item in list(value.items())[:128]
            if key in SAFE_FIELDS
        }
    if isinstance(value, list):
        return [_safe_json(item, secrets, depth + 1) for item in value[:128]]
    if isinstance(value, str):
        if any(secret and secret in value for secret in secrets):
            return "<redacted>"
        return value[:512] if value.isprintable() else "<invalid>"
    if value is None or isinstance(value, (bool, int, float)):
        return value
    return None
class ContractClient:
    def __init__(
        self,
        base_url: str,
        bearer_token: str,
        *,
        mock_base_url: str | None = None,
        timeout_seconds: float = 10,
    ) -> None:
        if not bearer_token or any(character.isspace() for character in bearer_token):
            raise ContractError("bearer token is invalid")
        if not 0 < timeout_seconds <= 60:
            raise ContractError("request timeout is invalid")
        self._base = _base_url(base_url)
        self._mock_base = _base_url(mock_base_url) if mock_base_url else None
        self._token = bearer_token
        self._timeout = timeout_seconds
        self._opener = urllib.request.build_opener(_NoRedirect)
    def __repr__(self) -> str:
        return "ContractClient(<redacted>)"
    def _headers(self, response: Any, secrets: set[str]) -> dict[str, str]:
        captured: dict[str, str] = {}
        for name in SAFE_HEADERS:
            value = response.headers.get(name)
            if value is not None:
                safe = _safe_json(value, secrets)
                captured[name] = safe if isinstance(safe, str) else "<invalid>"
        return captured
    @staticmethod
    def _read(response: Any, maximum: int) -> bytes:
        try:
            value = response.read(maximum + 1)
        except (OSError, http.client.HTTPException):
            raise ContractError("HTTP response read failed") from None
        if len(value) > maximum:
            raise ContractError("HTTP response exceeds the byte limit")
        return value
    def _open(self, request: urllib.request.Request) -> Any:
        try:
            return self._opener.open(request, timeout=self._timeout)
        except urllib.error.HTTPError as error:
            return error
        except (OSError, urllib.error.URLError) as error:
            raise ContractError("HTTP request failed") from None
    def _request_json(
        self,
        operation: str,
        method: str,
        path: str,
        *,
        payload: object = _ABSENT,
        idempotency_key: str | None = None,
        extra_headers: dict[str, str] | None = None,
        authenticated: bool = True,
        mock: bool = False,
    ) -> dict[str, object]:
        if not path.startswith("/") or path.startswith("//"):
            raise ContractError("request path is invalid")
        encoded = None
        secrets = {self._token}
        if payload is not _ABSENT:
            try:
                encoded = json.dumps(payload, sort_keys=True, separators=(",", ":"), allow_nan=False).encode()
            except (TypeError, ValueError):
                raise ContractError("request body is not canonical JSON") from None
            if len(encoded) > MAX_REQUEST_BYTES:
                raise ContractError("request body exceeds the byte limit")
            secrets.update(_secret_strings(payload))
        headers = {"Accept": "application/json"}
        if authenticated:
            headers["Authorization"] = f"Bearer {self._token}"
        if encoded is not None:
            headers["Content-Type"] = "application/json"
        if idempotency_key is not None:
            if not 1 <= len(idempotency_key) <= 128 or any(
                ord(char) < 33 or ord(char) > 126 for char in idempotency_key
            ):
                raise ContractError("idempotency key is invalid")
            headers["Idempotency-Key"] = idempotency_key
            secrets.add(idempotency_key)
        headers.update(extra_headers or {})
        root = self._mock_base if mock else self._base
        if root is None:
            raise ContractError("mock base URL is not configured")
        request = urllib.request.Request(root + path, data=encoded, headers=headers, method=method)
        response = self._open(request)
        try:
            raw = self._read(response, MAX_JSON_BYTES)
            try:
                parsed = json.loads(raw, parse_constant=lambda _value: None) if raw else None
            except (ValueError, UnicodeDecodeError, RecursionError):
                parsed = None
            captured_headers = self._headers(response, secrets)
            return {
                "operation": operation,
                "status": response.status,
                "receiver": captured_headers.get("x-ironcrew-instance-id"),
                "headers": captured_headers,
                "response": _safe_json(parsed, secrets),
                "response_bytes": len(raw),
            }
        finally:
            response.close()
    def sample_capabilities(self, samples: int, pause_seconds: float = 0) -> list[dict[str, object]]:
        if not 1 <= samples <= MAX_CAPABILITY_SAMPLES or not 0 <= pause_seconds <= 10:
            raise ContractError("capability sampling bounds are invalid")
        records = []
        for index in range(samples):
            records.append(self._request_json("capabilities", "GET", "/capabilities"))
            if pause_seconds and index + 1 < samples:
                time.sleep(pause_seconds)
        return records
    def _run(self, operation: str, flow: str, payload: object, key: str) -> dict[str, object]:
        path = f"/flows/{_component(flow, 'flow')}/run"
        return self._request_json(operation, "POST", path, payload=payload, idempotency_key=key)
    def start_run(self, flow: str, payload: object, key: str) -> dict[str, object]:
        return self._run("run_start", flow, payload, key)
    def replay_run(self, flow: str, payload: object, key: str) -> dict[str, object]:
        return self._run("run_replay", flow, payload, key)
    def conflict_run(self, flow: str, payload: object, key: str) -> dict[str, object]:
        return self._run("run_conflict", flow, payload, key)
    def poll_questions(self, flow: str, run_id: str) -> dict[str, object]:
        path = f"/flows/{_component(flow, 'flow')}/questions/{_component(run_id, 'run id')}"
        return self._request_json("question_poll", "GET", path)
    def wait_for_question(self, flow: str, run_id: str, attempts: int, pause_seconds: float = 0) -> dict[str, object]:
        if not 1 <= attempts <= MAX_POLL_ATTEMPTS or not 0 <= pause_seconds <= 10:
            raise ContractError("question polling bounds are invalid")
        latest: dict[str, object] = {}
        for index in range(attempts):
            latest = self.poll_questions(flow, run_id)
            response = latest.get("response")
            if isinstance(response, dict) and response.get("questions"):
                return latest
            if pause_seconds and index + 1 < attempts:
                time.sleep(pause_seconds)
        return latest
    def answer_question(self, flow: str, run_id: str, question_id: str, answer: object) -> dict[str, object]:
        path = f"/flows/{_component(flow, 'flow')}/answer/{_component(run_id, 'run id')}"
        return self._request_json(
            "question_answer", "POST", path,
            payload={"question_id": question_id, "answer": answer},
        )
    def abort_run(self, flow: str, run_id: str) -> dict[str, object]:
        path = f"/flows/{_component(flow, 'flow')}/abort/{_component(run_id, 'run id')}"
        return self._request_json("run_abort", "POST", path)
    def cursor_error(self, flow: str, run_id: str, cursor: str) -> dict[str, object]:
        if not 1 <= len(cursor) <= 256 or not cursor.isascii() or any(char in "\r\n" for char in cursor):
            raise ContractError("SSE cursor is invalid")
        path = f"/flows/{_component(flow, 'flow')}/events/{_component(run_id, 'run id')}"
        return self._request_json("sse_cursor_error", "GET", path, extra_headers={"Last-Event-ID": cursor})
    def mock_counts(self) -> dict[str, object]:
        return self._request_json("mock_counts", "GET", "/counts", authenticated=False, mock=True)
    def mock_reset(self) -> dict[str, object]:
        return self._request_json("mock_reset", "POST", "/reset", authenticated=False, mock=True)
    def collect_sse(
        self, flow: str, run_id: str, *, last_event_id: str | None = None,
        max_events: int = 64, max_bytes: int = MAX_SSE_BYTES,
    ) -> dict[str, object]:
        if not 1 <= max_events <= MAX_SSE_EVENTS or not 1 <= max_bytes <= MAX_SSE_BYTES:
            raise ContractError("SSE collection bounds are invalid")
        path = f"/flows/{_component(flow, 'flow')}/events/{_component(run_id, 'run id')}"
        headers = {"Accept": "text/event-stream", "Authorization": f"Bearer {self._token}"}
        if last_event_id is not None:
            if not 1 <= len(last_event_id) <= 256 or not last_event_id.isascii() or any(
                char in "\r\n" for char in last_event_id
            ):
                raise ContractError("SSE cursor is invalid")
            headers["Last-Event-ID"] = last_event_id
        response = self._open(urllib.request.Request(self._base + path, headers=headers, method="GET"))
        secrets = {self._token, last_event_id or ""}
        try:
            captured_headers = self._headers(response, secrets)
            if response.status != 200:
                raw = self._read(response, MAX_JSON_BYTES)
                try:
                    parsed = json.loads(raw, parse_constant=lambda _value: None)
                except (ValueError, UnicodeDecodeError, RecursionError):
                    parsed = None
                return {"operation": "sse_collect", "status": response.status,
                        "receiver": captured_headers.get("x-ironcrew-instance-id"),
                        "headers": captured_headers, "response": _safe_json(parsed, secrets),
                        "response_bytes": len(raw), "frames": []}
            frames: list[dict[str, object]] = []
            current: dict[str, object] = {"event": "message", "data_bytes": 0}
            total = 0
            while len(frames) < max_events:
                try:
                    line = response.readline(MAX_SSE_LINE_BYTES + 1)
                except (OSError, socket.timeout, TimeoutError):
                    raise ContractError("SSE collection timed out") from None
                if not line:
                    if current.get("id") is not None or current["data_bytes"]:
                        frames.append(current)
                    break
                total += len(line)
                if len(line) > MAX_SSE_LINE_BYTES or total > max_bytes:
                    raise ContractError("SSE response exceeds the byte limit")
                line = line.rstrip(b"\r\n")
                if not line:
                    if current.get("id") is not None or current["data_bytes"]:
                        frames.append(current)
                    current = {"event": "message", "data_bytes": 0}
                    continue
                field, separator, value = line.partition(b":")
                value = value[1:] if separator and value.startswith(b" ") else value
                if field == b"data":
                    current["data_bytes"] = int(current["data_bytes"]) + len(value)
                elif field in {b"id", b"event"}:
                    try:
                        decoded = value.decode("ascii")
                    except UnicodeDecodeError:
                        raise ContractError("SSE metadata is invalid") from None
                    current[field.decode()] = decoded[:512]
            return {"operation": "sse_collect", "status": response.status,
                    "receiver": captured_headers.get("x-ironcrew-instance-id"),
                    "headers": captured_headers, "frames": frames, "response_bytes": total,
                    "last_event_id": next((frame["id"] for frame in reversed(frames) if "id" in frame), None)}
        finally:
            response.close()
    def reconnect_sse(self, flow: str, run_id: str, cursor: str, **bounds: int) -> dict[str, object]:
        return self.collect_sse(flow, run_id, last_event_id=cursor, **bounds)
