from __future__ import annotations

import contextlib
import io
import json
import sys
import unittest
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any


sys.path.insert(0, str(Path(__file__).resolve().parent))

from mock_provider import (  # noqa: E402
    FINAL_OUTPUT,
    MAX_REQUEST_BYTES,
    MODEL,
    ProviderFixture,
)


def request_json(url: str, method: str = "GET", value: object | None = None) -> tuple[int, Any]:
    data = None if value is None else json.dumps(value).encode()
    request = urllib.request.Request(
        url,
        data=data,
        method=method,
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=5) as response:
        return response.status, json.load(response)


def provider_body(messages: list[dict[str, Any]]) -> dict[str, Any]:
    return {
        "model": MODEL,
        "messages": messages,
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "http_request",
                    "description": "bounded test tool",
                    "parameters": {"type": "object"},
                },
            }
        ],
    }


class MockProviderTests(unittest.TestCase):
    def test_tool_round_records_exactly_one_external_effect(self) -> None:
        with ProviderFixture() as provider:
            first_messages = [{"role": "user", "content": "perform the effect"}]
            status, first = request_json(
                f"{provider.base_url}/chat/completions",
                "POST",
                provider_body(first_messages),
            )
            self.assertEqual(status, 200)
            choice = first["choices"][0]
            self.assertEqual(choice["finish_reason"], "tool_calls")
            tool_call = choice["message"]["tool_calls"][0]
            self.assertEqual(tool_call["function"]["name"], "http_request")
            arguments = json.loads(tool_call["function"]["arguments"])
            self.assertEqual(arguments, {"method": "POST", "url": provider.effect_url})

            status, effect = request_json(provider.effect_url, "POST", None)
            self.assertEqual(status, 200)
            self.assertEqual(effect, {"effect": "recorded", "effect_count": 1})

            second_messages = [
                *first_messages,
                choice["message"],
                {
                    "role": "tool",
                    "tool_call_id": tool_call["id"],
                    "content": json.dumps(effect, sort_keys=True),
                },
            ]
            status, final = request_json(
                f"{provider.base_url}/chat/completions",
                "POST",
                provider_body(second_messages),
            )
            self.assertEqual(status, 200)
            self.assertEqual(final["choices"][0]["message"]["content"], FINAL_OUTPUT)

            _, counts = request_json(provider.base_url.removesuffix("/v1") + "/counts")
            self.assertEqual(
                counts,
                {
                    "chat_completions": 2,
                    "effect_calls": 1,
                    "final_responses": 1,
                    "tool_call_responses": 1,
                },
            )

    def test_counts_and_reset_have_fixed_shapes(self) -> None:
        with ProviderFixture() as provider:
            root = provider.base_url.removesuffix("/v1")
            request_json(provider.effect_url, "POST", None)
            _, reset = request_json(f"{root}/reset", "POST", None)
            _, counts = request_json(f"{root}/counts")
            expected = {
                "chat_completions": 0,
                "effect_calls": 0,
                "final_responses": 0,
                "tool_call_responses": 0,
            }
            self.assertEqual(reset, expected)
            self.assertEqual(counts, expected)

    def test_invalid_contract_and_oversized_body_are_rejected(self) -> None:
        with ProviderFixture() as provider:
            url = f"{provider.base_url}/chat/completions"
            with self.assertRaises(urllib.error.HTTPError) as missing_tool:
                request_json(
                    url,
                    "POST",
                    {"model": MODEL, "messages": [{"role": "user", "content": "x"}]},
                )
            self.assertEqual(missing_tool.exception.code, 400)
            missing_tool.exception.close()

            request = urllib.request.Request(
                url,
                data=b"x" * (MAX_REQUEST_BYTES + 1),
                method="POST",
                headers={"Content-Type": "application/json"},
            )
            with self.assertRaises(urllib.error.HTTPError) as oversized:
                urllib.request.urlopen(request, timeout=5)
            self.assertEqual(oversized.exception.code, 413)
            oversized.exception.close()

    def test_requests_do_not_log_bodies(self) -> None:
        marker = "request-body-must-never-be-logged"
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr), ProviderFixture() as provider:
            with self.assertRaises(urllib.error.HTTPError) as rejected:
                request_json(
                    f"{provider.base_url}/chat/completions",
                    "POST",
                    {"model": MODEL, "messages": marker},
                )
            rejected.exception.close()
        self.assertNotIn(marker, stderr.getvalue())
        self.assertEqual(stderr.getvalue(), "")


if __name__ == "__main__":
    unittest.main()
