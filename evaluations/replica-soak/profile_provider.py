"""Bounded loopback OpenAI-compatible fixture for IC-018 workload profiles."""

from __future__ import annotations

import json
import socket
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any


PROVIDER_TOOL_MODEL = "ic018-provider-tool"
LARGE_RESULT_MODEL = "ic018-large-result"
CONVERSATION_MODEL = "ic018-conversation"
PROVIDER_TOOL_OUTPUT = "ic018-provider-tool-ok"
CONVERSATION_OUTPUT = "ic018-conversation-ok"
LARGE_RESULT_BYTES = 64 * 1024
MAX_REQUEST_BYTES = 1024 * 1024
MAX_HEADER_BYTES = 32 * 1024
MAX_CONCURRENT_REQUESTS = 8
REQUEST_TIMEOUT_SECONDS = 10


class Counters:
    """Fixed-name counters that never retain request or response content."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._values = {
            "provider_tool_provider_calls": 0,
            "provider_tool_tool_effects": 0,
            "large_result_provider_calls": 0,
            "conversation_provider_calls": 0,
            "invalid_requests": 0,
        }

    def increment(self, name: str) -> int:
        with self._lock:
            self._values[name] += 1
            return self._values[name]

    def snapshot(self) -> dict[str, int]:
        with self._lock:
            return dict(self._values)


class ProfileProvider(ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = 16

    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), ProfileProviderHandler)
        self.counters = Counters()
        self._slots = threading.BoundedSemaphore(MAX_CONCURRENT_REQUESTS)

    @property
    def base_url(self) -> str:
        host, port = self.server_address[:2]
        return f"http://{host}:{port}/v1"

    @property
    def effect_url(self) -> str:
        host, port = self.server_address[:2]
        return f"http://{host}:{port}/effect"

    def process_request(self, request: socket.socket, client_address: Any) -> None:
        if not self._slots.acquire(blocking=False):
            request.close()
            return
        try:
            super().process_request(request, client_address)
        except BaseException:
            self._slots.release()
            raise

    def process_request_thread(self, request: socket.socket, client_address: Any) -> None:
        try:
            super().process_request_thread(request, client_address)
        finally:
            self._slots.release()


class ProfileProviderHandler(BaseHTTPRequestHandler):
    server: ProfileProvider
    protocol_version = "HTTP/1.1"

    def setup(self) -> None:
        super().setup()
        self.connection.settimeout(REQUEST_TIMEOUT_SECONDS)

    def log_message(self, _format: str, *_args: object) -> None:
        return

    def _send_json(self, status: int, value: dict[str, Any]) -> None:
        payload = json.dumps(value, separators=(",", ":"), sort_keys=True).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(payload)

    def _reject(self, status: int, message: str) -> None:
        self.server.counters.increment("invalid_requests")
        self._send_json(status, {"error": {"message": message}})

    def _content_length(self, maximum: int = MAX_REQUEST_BYTES) -> int | None:
        header_bytes = sum(len(name) + len(value) + 4 for name, value in self.headers.items())
        if header_bytes > MAX_HEADER_BYTES:
            self._reject(431, "headers out of bounds")
            return None
        try:
            length = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            self._reject(400, "invalid content length")
            return None
        if not 0 <= length <= maximum:
            self._reject(413, "request body out of bounds")
            return None
        return length

    def _read_json(self) -> dict[str, Any] | None:
        length = self._content_length()
        if length is None:
            return None
        if length == 0:
            self._reject(400, "JSON body required")
            return None
        try:
            value = json.loads(self.rfile.read(length))
        except (json.JSONDecodeError, TimeoutError, socket.timeout):
            self._reject(400, "invalid JSON body")
            return None
        if not isinstance(value, dict):
            self._reject(400, "JSON object required")
            return None
        return value

    def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler contract
        if self.path.rstrip("/") == "/counts":
            self._send_json(200, self.server.counters.snapshot())
            return
        self._reject(404, "not found")

    def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler contract
        path = self.path.rstrip("/")
        if path == "/effect":
            if self._content_length(maximum=0) is None:
                return
            ordinal = self.server.counters.increment("provider_tool_tool_effects")
            self._send_json(200, {"effect": "recorded", "ordinal": ordinal})
            return
        if path != "/v1/chat/completions":
            self._reject(404, "not found")
            return
        body = self._read_json()
        if body is not None:
            self._chat_completion(body)

    def _chat_completion(self, body: dict[str, Any]) -> None:
        model = body.get("model")
        messages = body.get("messages")
        if model not in {
            PROVIDER_TOOL_MODEL,
            LARGE_RESULT_MODEL,
            CONVERSATION_MODEL,
        } or not isinstance(messages, list):
            self._reject(400, "unexpected provider contract")
            return

        if model == PROVIDER_TOOL_MODEL:
            self._provider_tool(body, messages)
            return
        if model == LARGE_RESULT_MODEL:
            self.server.counters.increment("large_result_provider_calls")
            self._completion("L" * LARGE_RESULT_BYTES)
            return
        self.server.counters.increment("conversation_provider_calls")
        self._completion(CONVERSATION_OUTPUT)

    def _provider_tool(
        self, body: dict[str, Any], messages: list[object]
    ) -> None:
        tools = body.get("tools")
        offered = isinstance(tools, list) and any(
            isinstance(tool, dict)
            and isinstance(tool.get("function"), dict)
            and tool["function"].get("name") == "http_request"
            for tool in tools
        )
        if not offered:
            self._reject(400, "http_request tool not offered")
            return
        ordinal = self.server.counters.increment("provider_tool_provider_calls")
        has_tool_result = any(
            isinstance(message, dict) and message.get("role") == "tool"
            for message in messages
        )
        if not has_tool_result:
            self._completion(
                None,
                finish_reason="tool_calls",
                tool_calls=[
                    {
                        "id": f"ic018-effect-{ordinal}",
                        "type": "function",
                        "function": {
                            "name": "http_request",
                            "arguments": json.dumps(
                                {"url": self.server.effect_url, "method": "POST"},
                                separators=(",", ":"),
                                sort_keys=True,
                            ),
                        },
                    }
                ],
            )
            return
        self._completion(PROVIDER_TOOL_OUTPUT)

    def _completion(
        self,
        content: str | None,
        *,
        finish_reason: str = "stop",
        tool_calls: list[dict[str, Any]] | None = None,
    ) -> None:
        message: dict[str, Any] = {"role": "assistant", "content": content}
        if tool_calls is not None:
            message["tool_calls"] = tool_calls
        self._send_json(
            200,
            {
                "id": "ic018-profile-fixture",
                "object": "chat.completion",
                "choices": [
                    {"index": 0, "message": message, "finish_reason": finish_reason}
                ],
                "usage": {
                    "prompt_tokens": 1,
                    "completion_tokens": 1,
                    "total_tokens": 2,
                },
            },
        )


class ProviderFixture:
    def __init__(self) -> None:
        self.server = ProfileProvider()
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def __enter__(self) -> ProfileProvider:
        self.thread.start()
        return self.server

    def __exit__(self, *_args: object) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)
