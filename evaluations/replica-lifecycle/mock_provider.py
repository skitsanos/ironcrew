"""Bounded loopback OpenAI-compatible provider for IC-020 capacity evidence."""

from __future__ import annotations

import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any


MAX_REQUEST_BYTES = 1024 * 1024
GATE_TIMEOUT_SECONDS = 20.0


class ProviderGate:
    def __init__(self) -> None:
        self._condition = threading.Condition()
        self._release = threading.Event()
        self._label = "idle"
        self._expected = 0
        self._active = 0
        self._arrivals = 0
        self._peak = 0
        self._failed = 0

    def begin(self, label: str, expected: int) -> None:
        if expected < 1:
            raise ValueError("provider phase must expect at least one call")
        with self._condition:
            if self._active != 0:
                raise RuntimeError("provider phase began while calls were active")
            self._label = label
            self._expected = expected
            self._arrivals = 0
            self._peak = 0
            self._failed = 0
            self._release.clear()

    def enter(self) -> tuple[str, bool]:
        with self._condition:
            label = self._label
            self._active += 1
            self._arrivals += 1
            self._peak = max(self._peak, self._active)
            self._condition.notify_all()
        released = self._release.wait(GATE_TIMEOUT_SECONDS)
        return label, released

    def leave(self, released: bool) -> None:
        with self._condition:
            if not released:
                self._failed += 1
            self._active -= 1
            self._condition.notify_all()

    def wait_saturated(self, timeout: float) -> None:
        deadline = time.monotonic() + timeout
        with self._condition:
            while self._active < self._expected:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError(
                        f"provider saturation timed out at {self._active}/{self._expected}"
                    )
                self._condition.wait(remaining)
            if self._active != self._expected:
                raise RuntimeError(
                    f"provider concurrency exceeded {self._expected}: {self._active}"
                )

    def release(self) -> None:
        self._release.set()

    def wait_idle(self, timeout: float) -> None:
        deadline = time.monotonic() + timeout
        with self._condition:
            while self._active:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError("provider calls did not leave the bounded gate")
                self._condition.wait(remaining)

    def snapshot(self) -> dict[str, Any]:
        with self._condition:
            return {
                "label": self._label,
                "expected_calls": self._expected,
                "arrivals": self._arrivals,
                "peak_active_calls": self._peak,
                "active_calls": self._active,
                "failed_calls": self._failed,
            }


class CapacityProvider(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), CapacityHandler)
        self.gate = ProviderGate()

    @property
    def base_url(self) -> str:
        host, port = self.server_address[:2]
        return f"http://{host}:{port}/v1"


class CapacityHandler(BaseHTTPRequestHandler):
    server: CapacityProvider
    protocol_version = "HTTP/1.1"

    def log_message(self, _format: str, *_args: object) -> None:
        return

    def send_json(self, status: int, value: dict[str, Any]) -> None:
        payload = json.dumps(value, separators=(",", ":"), sort_keys=True).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(payload)

    def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler contract
        if self.path.rstrip("/") != "/v1/chat/completions":
            self.send_json(404, {"error": {"message": "not found"}})
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            length = 0
        if not 0 < length <= MAX_REQUEST_BYTES:
            self.send_json(413, {"error": {"message": "request out of bounds"}})
            return
        try:
            body = json.loads(self.rfile.read(length))
            if body.get("model") != "ic020-loopback" or not isinstance(
                body.get("messages"), list
            ):
                raise ValueError("unexpected provider request contract")
        except (json.JSONDecodeError, ValueError, AttributeError) as error:
            self.send_json(400, {"error": {"message": str(error)}})
            return

        _label, released = self.server.gate.enter()
        try:
            if not released:
                self.send_json(503, {"error": {"message": "provider gate timed out"}})
                return
            self.send_json(
                200,
                {
                    "id": "ic020-capacity",
                    "object": "chat.completion",
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": "ic020-capacity-ok",
                            },
                            "finish_reason": "stop",
                        }
                    ],
                    "usage": {
                        "prompt_tokens": 1,
                        "completion_tokens": 1,
                        "total_tokens": 2,
                    },
                },
            )
        finally:
            self.server.gate.leave(released)


class ProviderFixture:
    def __init__(self) -> None:
        self.server = CapacityProvider()
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def __enter__(self) -> CapacityProvider:
        self.thread.start()
        return self.server

    def __exit__(self, *_args: object) -> None:
        self.server.gate.release()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)
