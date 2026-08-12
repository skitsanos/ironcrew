#!/usr/bin/env python3
"""Bounded OpenAI-compatible mock with an external-effect counter."""

from __future__ import annotations

import argparse
import json
import socket
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any


MODEL = "ic007-platform-canary"
FINAL_OUTPUT = "ic007-platform-effect-recorded"
MAX_REQUEST_BYTES = 256 * 1024
MAX_HEADER_BYTES = 32 * 1024
MAX_CONCURRENT_REQUESTS = 8
REQUEST_TIMEOUT_SECONDS = 10


class Counters:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._values = self._empty()

    @staticmethod
    def _empty() -> dict[str, int]:
        return {
            "chat_completions": 0,
            "effect_calls": 0,
            "final_responses": 0,
            "tool_call_responses": 0,
        }

    def increment(self, name: str) -> int:
        with self._lock:
            self._values[name] += 1
            return self._values[name]

    def reset(self) -> dict[str, int]:
        with self._lock:
            self._values = self._empty()
            return dict(self._values)

    def snapshot(self) -> dict[str, int]:
        with self._lock:
            return dict(self._values)


class PlatformCanaryServer(ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = 16

    def __init__(
        self,
        address: tuple[str, int],
        effect_url: str | None = None,
    ) -> None:
        super().__init__(address, PlatformCanaryHandler)
        host, port = self.server_address[:2]
        self.effect_url = effect_url or f"http://{host}:{port}/effect"
        self.counters = Counters()
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


class PlatformCanaryHandler(BaseHTTPRequestHandler):
    server: PlatformCanaryServer
    protocol_version = "HTTP/1.1"

    def setup(self) -> None:
        super().setup()
        self.connection.settimeout(REQUEST_TIMEOUT_SECONDS)

    def log_message(self, _format: str, *_args: object) -> None:
        return

    def _send_json(self, status: int, value: dict[str, Any]) -> None:
        payload = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(payload)

    def _content_length(self, maximum: int = MAX_REQUEST_BYTES) -> int | None:
        header_bytes = sum(len(name) + len(value) + 4 for name, value in self.headers.items())
        if header_bytes > MAX_HEADER_BYTES:
            self._send_json(431, {"error": {"message": "headers out of bounds"}})
            return None
        raw = self.headers.get("Content-Length", "0")
        try:
            length = int(raw)
        except ValueError:
            self._send_json(400, {"error": {"message": "invalid content length"}})
            return None
        if length < 0 or length > maximum:
            self._send_json(413, {"error": {"message": "request body out of bounds"}})
            return None
        return length

    def _read_json(self) -> dict[str, Any] | None:
        length = self._content_length()
        if length is None:
            return None
        if length == 0:
            self._send_json(400, {"error": {"message": "JSON body required"}})
            return None
        try:
            value = json.loads(self.rfile.read(length))
        except (json.JSONDecodeError, TimeoutError, socket.timeout):
            self._send_json(400, {"error": {"message": "invalid JSON body"}})
            return None
        if not isinstance(value, dict):
            self._send_json(400, {"error": {"message": "JSON object required"}})
            return None
        return value

    def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler contract
        if self.path.rstrip("/") == "/counts":
            self._send_json(200, self.server.counters.snapshot())
        else:
            self._send_json(404, {"error": {"message": "not found"}})

    def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler contract
        path = self.path.rstrip("/")
        if path == "/reset":
            if self._content_length(maximum=0) is not None:
                self._send_json(200, self.server.counters.reset())
            return
        if path == "/effect":
            if self._content_length(maximum=0) is None:
                return
            count = self.server.counters.increment("effect_calls")
            self._send_json(200, {"effect": "recorded", "effect_count": count})
            return
        if path != "/v1/chat/completions":
            self._send_json(404, {"error": {"message": "not found"}})
            return
        body = self._read_json()
        if body is not None:
            self._chat_completion(body)

    def _chat_completion(self, body: dict[str, Any]) -> None:
        messages = body.get("messages")
        tools = body.get("tools")
        if body.get("model") != MODEL or not isinstance(messages, list):
            self._send_json(400, {"error": {"message": "unexpected provider contract"}})
            return
        offered = isinstance(tools, list) and any(
            isinstance(tool, dict)
            and isinstance(tool.get("function"), dict)
            and tool["function"].get("name") == "http_request"
            for tool in tools
        )
        has_tool_result = any(
            isinstance(message, dict) and message.get("role") == "tool"
            for message in messages
        )
        if not offered:
            self._send_json(400, {"error": {"message": "http_request tool not offered"}})
            return

        self.server.counters.increment("chat_completions")
        if not has_tool_result:
            ordinal = self.server.counters.increment("tool_call_responses")
            call_id = f"ic007-effect-{ordinal}"
            message = {
                "role": "assistant",
                "content": None,
                "tool_calls": [
                    {
                        "id": call_id,
                        "type": "function",
                        "function": {
                            "name": "http_request",
                            "arguments": json.dumps(
                                {"url": self.server.effect_url, "method": "POST"},
                                sort_keys=True,
                                separators=(",", ":"),
                            ),
                        },
                    }
                ],
            }
            finish_reason = "tool_calls"
        else:
            self.server.counters.increment("final_responses")
            message = {"role": "assistant", "content": FINAL_OUTPUT}
            finish_reason = "stop"
        self._send_json(
            200,
            {
                "id": "ic007-platform-canary",
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
        self.server = PlatformCanaryServer(("127.0.0.1", 0))
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def __enter__(self) -> PlatformCanaryServer:
        self.thread.start()
        return self.server

    def __exit__(self, *_args: object) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--effect-url")
    args = parser.parse_args()
    server = PlatformCanaryServer((args.host, args.port), args.effect_url)
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
