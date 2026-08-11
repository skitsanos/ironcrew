from __future__ import annotations

import contextlib
import io
import json
import sys
import threading
import time
import unittest
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any


sys.path.insert(0, str(Path(__file__).resolve().parent))

from ic008_mock_provider import (  # noqa: E402
    API_KEY,
    MAX_REQUEST_BYTES,
    MODEL,
    Ic008MockServer,
)


def request_json(
    url: str,
    method: str = "GET",
    value: object | None = None,
    *,
    authorized: bool = True,
) -> tuple[int, Any]:
    data = None if value is None else json.dumps(value).encode()
    headers = {"Content-Type": "application/json"}
    if authorized:
        headers["Authorization"] = f"Bearer {API_KEY}"
    request = urllib.request.Request(url, data=data, method=method, headers=headers)
    try:
        response = urllib.request.urlopen(request, timeout=5)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        return response.status, json.load(response)


def provider_body(messages: list[dict[str, Any]]) -> dict[str, Any]:
    return {
        "model": MODEL,
        "messages": messages,
        "tools": [{
            "type": "function",
            "function": {
                "name": "http_request",
                "description": "bounded test effect",
                "parameters": {"type": "object"},
            },
        }],
    }


class Fixture:
    def __init__(
        self,
        blocking_content: str | None = None,
        gate_timeout_seconds: float = 2,
    ) -> None:
        self.server = Ic008MockServer(
            ("127.0.0.1", 0),
            blocking_content=blocking_content,
            gate_timeout_seconds=gate_timeout_seconds,
        )
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def __enter__(self) -> Ic008MockServer:
        self.thread.start()
        return self.server

    def __exit__(self, *_args: object) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


def complete_turn(
    provider: Ic008MockServer,
    history: list[dict[str, Any]],
    content: str,
) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    messages = [*history, {"role": "user", "content": content}]
    status, first = request_json(
        f"{provider.base_url}/chat/completions",
        "POST",
        provider_body(messages),
    )
    if status != 200:
        raise AssertionError(first)
    choice = first["choices"][0]
    tool_call = choice["message"]["tool_calls"][0]
    arguments = json.loads(tool_call["function"]["arguments"])
    status, effect = request_json(arguments["url"], "POST")
    if status != 200:
        raise AssertionError(effect)
    messages.extend([
        choice["message"],
        {
            "role": "tool",
            "tool_call_id": tool_call["id"],
            "content": json.dumps(effect, sort_keys=True),
        },
    ])
    status, final = request_json(
        f"{provider.base_url}/chat/completions",
        "POST",
        provider_body(messages),
    )
    if status != 200:
        raise AssertionError(final)
    messages.append(final["choices"][0]["message"])
    return messages, final


class Ic008MockProviderTests(unittest.TestCase):
    def test_each_conversation_turn_echoes_and_counts_one_effect(self) -> None:
        with Fixture() as provider:
            history, first = complete_turn(provider, [], "cold-peer-turn")
            self.assertEqual(
                first["choices"][0]["message"]["content"],
                "mock:cold-peer-turn",
            )
            _history, second = complete_turn(provider, history, "owner-death-turn")
            self.assertEqual(
                second["choices"][0]["message"]["content"],
                "mock:owner-death-turn",
            )
            status, counts = request_json(
                provider.base_url.removesuffix("/v1") + "/counts"
            )
            self.assertEqual(status, 200)
            self.assertEqual(counts, {
                "chat_completions": 4,
                "effect_calls": 2,
                "final_responses": 2,
                "tool_call_responses": 2,
            })

    def test_content_gate_reports_blocks_and_releases_exactly_one_request(self) -> None:
        with Fixture("block-delete") as provider:
            url = f"{provider.base_url}/chat/completions"
            status, _ = request_json(
                url,
                "POST",
                provider_body([{"role": "user", "content": "not-blocked"}]),
            )
            self.assertEqual(status, 200)

            result: list[tuple[int, Any]] = []
            blocked = threading.Thread(
                target=lambda: result.append(request_json(
                    url,
                    "POST",
                    provider_body([{"role": "user", "content": "block-delete"}]),
                )),
                daemon=True,
            )
            blocked.start()
            root = provider.base_url.removesuffix("/v1")
            deadline = time.monotonic() + 2
            gate: dict[str, Any] = {}
            while time.monotonic() < deadline:
                _, gate = request_json(f"{root}/status")
                if gate["blocked"]:
                    break
                time.sleep(0.01)
            self.assertEqual(gate["blocked_requests"], 1)
            self.assertEqual(gate["chat_completions"], 2)
            self.assertEqual(gate["tool_call_responses"], 1)
            self.assertNotIn("blocking_content", gate)

            status, released = request_json(f"{root}/release", "POST")
            self.assertEqual(status, 200)
            self.assertEqual(released, {
                "release_generation": 1,
                "released_requests": 1,
            })
            blocked.join(timeout=2)
            self.assertFalse(blocked.is_alive())
            self.assertEqual(result[0][0], 200)
            _, settled = request_json(f"{root}/status")
            self.assertFalse(settled["blocked"])
            self.assertEqual(settled["tool_call_responses"], 2)
            self.assertEqual(settled["effect_calls"], 0)

    def test_content_gate_times_out_with_a_bounded_error(self) -> None:
        with Fixture("block-delete", gate_timeout_seconds=0.05) as provider:
            status, body = request_json(
                f"{provider.base_url}/chat/completions",
                "POST",
                provider_body([{"role": "user", "content": "block-delete"}]),
            )
            self.assertEqual((status, body), (504, {"error": "provider gate timed out"}))
            _, gate = request_json(provider.base_url.removesuffix("/v1") + "/status")
            self.assertFalse(gate["blocked"])
            self.assertEqual(gate["chat_completions"], 1)
            self.assertEqual(gate["tool_call_responses"], 0)

    def test_auth_contract_and_request_size_fail_closed_without_logging_bodies(self) -> None:
        marker = "request-body-must-never-be-logged"
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr), Fixture() as provider:
            url = f"{provider.base_url}/chat/completions"
            status, _ = request_json(
                url,
                "POST",
                provider_body([{"role": "user", "content": marker}]),
                authorized=False,
            )
            self.assertEqual(status, 401)
            status, _ = request_json(
                url,
                "POST",
                {"model": MODEL, "messages": [{"role": "user", "content": marker}]},
            )
            self.assertEqual(status, 400)
            request = urllib.request.Request(
                url,
                data=b"x" * (MAX_REQUEST_BYTES + 1),
                method="POST",
                headers={"Authorization": f"Bearer {API_KEY}"},
            )
            with self.assertRaises(urllib.error.HTTPError) as oversized:
                urllib.request.urlopen(request, timeout=5)
            self.assertEqual(oversized.exception.code, 413)
            oversized.exception.close()
        self.assertEqual(stderr.getvalue(), "")


if __name__ == "__main__":
    unittest.main()
