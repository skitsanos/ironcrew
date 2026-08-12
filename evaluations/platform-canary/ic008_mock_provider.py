#!/usr/bin/env python3
"""Bounded OpenAI-compatible IC-008 conversation/effect fixture."""

from __future__ import annotations

import argparse
import json
import socket
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

from ic008_mock_state import ProviderState


API_KEY = "ic008-platform-mock-key"
MODEL = "ic008-platform-canary"
MAX_REQUEST_BYTES = 256 * 1024
MAX_HEADER_BYTES = 32 * 1024
MAX_USER_CONTENT_BYTES = 16 * 1024
MAX_MESSAGES = 128
MAX_CHAT_COMPLETIONS = 32
MAX_EFFECT_CALLS = 16
MAX_CONCURRENT_REQUESTS = 8
REQUEST_TIMEOUT_SECONDS = 10
DEFAULT_GATE_TIMEOUT_SECONDS = 30.0


class Ic008MockServer(ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = 16

    def __init__(
        self,
        address: tuple[str, int],
        *,
        effect_url: str | None = None,
        blocking_content: str | None = None,
        gate_timeout_seconds: float = DEFAULT_GATE_TIMEOUT_SECONDS,
    ) -> None:
        if blocking_content is not None and not _valid_content(blocking_content):
            raise ValueError("blocking content is invalid")
        if not 0 < gate_timeout_seconds <= 60:
            raise ValueError("gate timeout is invalid")
        super().__init__(address, Ic008MockHandler)
        host, port = self.server_address[:2]
        self.effect_url = effect_url or f"http://{host}:{port}/effect"
        self.state = ProviderState(blocking_content, gate_timeout_seconds)
        self._slots = threading.BoundedSemaphore(MAX_CONCURRENT_REQUESTS)

    @property
    def base_url(self) -> str:
        host, port = self.server_address[:2]
        return f"http://{host}:{port}/v1"

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

    def handle_error(self, _request: socket.socket, _client_address: Any) -> None:
        return


class Ic008MockHandler(BaseHTTPRequestHandler):
    server: Ic008MockServer
    protocol_version = "HTTP/1.1"

    def setup(self) -> None:
        super().setup()
        self.connection.settimeout(REQUEST_TIMEOUT_SECONDS)

    def log_message(self, _format: str, *_args: object) -> None:
        return

    def _send_json(self, status: int, value: object) -> None:
        payload = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("Connection", "close")
        self.end_headers()
        try:
            self.wfile.write(payload)
        except OSError:
            pass

    def _content_length(self, maximum: int = MAX_REQUEST_BYTES) -> int | None:
        header_bytes = sum(len(name) + len(value) + 4 for name, value in self.headers.items())
        if header_bytes > MAX_HEADER_BYTES:
            self._send_json(431, {"error": "headers out of bounds"})
            return None
        if self.headers.get("Transfer-Encoding") is not None:
            self._send_json(400, {"error": "transfer encoding is unsupported"})
            return None
        try:
            length = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            self._send_json(400, {"error": "invalid content length"})
            return None
        if length < 0 or length > maximum:
            self._send_json(413, {"error": "request body out of bounds"})
            return None
        return length

    def _read_json(self) -> dict[str, Any] | None:
        length = self._content_length()
        if length is None:
            return None
        if length == 0:
            self._send_json(400, {"error": "JSON body required"})
            return None
        try:
            value = json.loads(self.rfile.read(length))
        except (json.JSONDecodeError, TimeoutError, socket.timeout):
            self._send_json(400, {"error": "invalid JSON body"})
            return None
        if not isinstance(value, dict):
            self._send_json(400, {"error": "JSON object required"})
            return None
        return value

    def _provider_authorized(self) -> bool:
        if self.headers.get("Authorization") == f"Bearer {API_KEY}":
            return True
        self._send_json(401, {"error": "unauthorized"})
        return False

    def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler contract
        path = self.path.rstrip("/")
        if path == "/counts":
            self._send_json(200, self.server.state.counts())
        elif path == "/status":
            self._send_json(200, self.server.state.status())
        else:
            self._send_json(404, {"error": "not found"})

    def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler contract
        path = self.path.rstrip("/")
        if path == "/release":
            if self._content_length(maximum=0) is not None:
                released, generation = self.server.state.release()
                self._send_json(
                    200,
                    {"release_generation": generation, "released_requests": released},
                )
            return
        if path == "/effect":
            if self._content_length(maximum=0) is None:
                return
            count = self.server.state.increment("effect_calls", MAX_EFFECT_CALLS)
            if count is None:
                self._send_json(429, {"error": "effect bound exceeded"})
            else:
                self._send_json(200, {"effect": "recorded", "effect_count": count})
            return
        if path != "/v1/chat/completions":
            self._send_json(404, {"error": "not found"})
            return
        if not self._provider_authorized():
            return
        body = self._read_json()
        if body is not None:
            self._chat_completion(body)

    def _chat_completion(self, body: dict[str, Any]) -> None:
        turn = _current_turn(body)
        if body.get("model") != MODEL or turn is None or not _offers_http_tool(body):
            self._send_json(400, {"error": "unexpected provider contract"})
            return
        content, has_tool_result = turn
        sequence = self.server.state.increment("chat_completions", MAX_CHAT_COMPLETIONS)
        if sequence is None:
            self._send_json(429, {"error": "provider request bound exceeded"})
            return
        if not has_tool_result:
            if not self.server.state.wait_if_blocked(content):
                self._send_json(504, {"error": "provider gate timed out"})
                return
            ordinal = self.server.state.increment(
                "tool_call_responses", MAX_CHAT_COMPLETIONS
            )
            assert ordinal is not None
            message: dict[str, Any] = {
                "role": "assistant",
                "content": None,
                "tool_calls": [{
                    "id": f"ic008-effect-{ordinal}",
                    "type": "function",
                    "function": {
                        "name": "http_request",
                        "arguments": json.dumps(
                            {"method": "POST", "url": self.server.effect_url},
                            sort_keys=True,
                            separators=(",", ":"),
                        ),
                    },
                }],
            }
            finish_reason = "tool_calls"
        else:
            ordinal = self.server.state.increment("final_responses", MAX_CHAT_COMPLETIONS)
            assert ordinal is not None
            message = {"role": "assistant", "content": f"mock:{content}"}
            finish_reason = "stop"
        self._send_json(200, {
            "id": f"ic008-{sequence}",
            "object": "chat.completion",
            "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        })


def _valid_content(value: object) -> bool:
    return (
        isinstance(value, str)
        and bool(value)
        and len(value.encode()) <= MAX_USER_CONTENT_BYTES
        and not any(character in "\r\n\0" for character in value)
    )


def _current_turn(body: dict[str, Any]) -> tuple[str, bool] | None:
    messages = body.get("messages")
    if not isinstance(messages, list) or not 1 <= len(messages) <= MAX_MESSAGES:
        return None
    user_indexes = [
        index
        for index, message in enumerate(messages)
        if isinstance(message, dict) and message.get("role") == "user"
    ]
    if not user_indexes:
        return None
    index = user_indexes[-1]
    content = messages[index].get("content")
    if not _valid_content(content):
        return None
    has_tool_result = any(
        isinstance(message, dict) and message.get("role") == "tool"
        for message in messages[index + 1 :]
    )
    return content, has_tool_result


def _offers_http_tool(body: dict[str, Any]) -> bool:
    tools = body.get("tools")
    return isinstance(tools, list) and any(
        isinstance(tool, dict)
        and isinstance(tool.get("function"), dict)
        and tool["function"].get("name") == "http_request"
        for tool in tools
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--effect-url")
    parser.add_argument("--blocking-content")
    parser.add_argument(
        "--gate-timeout-seconds", type=float, default=DEFAULT_GATE_TIMEOUT_SECONDS
    )
    args = parser.parse_args()
    try:
        server = Ic008MockServer(
            (args.host, args.port),
            effect_url=args.effect_url,
            blocking_content=args.blocking_content,
            gate_timeout_seconds=args.gate_timeout_seconds,
        )
    except (OSError, ValueError) as error:
        parser.error(str(error))
    print(server.base_url, flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
