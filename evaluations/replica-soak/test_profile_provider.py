import json
import sys
import unittest
import urllib.error
import urllib.request
from pathlib import Path


sys.path.insert(0, str(Path(__file__).resolve().parent))
from profile_provider import (  # noqa: E402
    CONVERSATION_MODEL,
    CONVERSATION_OUTPUT,
    LARGE_RESULT_BYTES,
    LARGE_RESULT_MODEL,
    PROVIDER_TOOL_MODEL,
    PROVIDER_TOOL_OUTPUT,
    ProviderFixture,
)


def request_json(url: str, payload: object | None = None) -> tuple[int, object]:
    data = None if payload is None else json.dumps(payload).encode()
    method = "GET" if payload is None else "POST"
    request = urllib.request.Request(
        url, data=data, headers={"Content-Type": "application/json"}, method=method
    )
    try:
        response = urllib.request.urlopen(request, timeout=5)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        return response.status, json.loads(response.read())


class ProfileProviderTests(unittest.TestCase):
    def test_fixture_serves_exact_bounded_profiles(self) -> None:
        with ProviderFixture() as provider:
            tools = [{"type": "function", "function": {"name": "http_request"}}]
            status, first = request_json(
                f"{provider.base_url}/chat/completions",
                {"model": PROVIDER_TOOL_MODEL, "messages": [], "tools": tools},
            )
            self.assertEqual(status, 200)
            tool_call = first["choices"][0]["message"]["tool_calls"][0]
            arguments = json.loads(tool_call["function"]["arguments"])
            self.assertEqual(arguments, {"url": provider.effect_url, "method": "POST"})

            effect_request = urllib.request.Request(
                provider.effect_url, data=b"", method="POST"
            )
            with urllib.request.urlopen(effect_request, timeout=5) as response:
                self.assertEqual(json.loads(response.read())["effect"], "recorded")
            status, final = request_json(
                f"{provider.base_url}/chat/completions",
                {
                    "model": PROVIDER_TOOL_MODEL,
                    "messages": [{"role": "tool", "content": "recorded"}],
                    "tools": tools,
                },
            )
            self.assertEqual(status, 200)
            self.assertEqual(final["choices"][0]["message"]["content"], PROVIDER_TOOL_OUTPUT)

            _, large = request_json(
                f"{provider.base_url}/chat/completions",
                {"model": LARGE_RESULT_MODEL, "messages": []},
            )
            self.assertEqual(len(large["choices"][0]["message"]["content"]), LARGE_RESULT_BYTES)
            _, conversation = request_json(
                f"{provider.base_url}/chat/completions",
                {"model": CONVERSATION_MODEL, "messages": []},
            )
            self.assertEqual(
                conversation["choices"][0]["message"]["content"], CONVERSATION_OUTPUT
            )
            self.assertEqual(
                provider.counters.snapshot(),
                {
                    "provider_tool_provider_calls": 2,
                    "provider_tool_tool_effects": 1,
                    "large_result_provider_calls": 1,
                    "conversation_provider_calls": 1,
                    "invalid_requests": 0,
                },
            )

    def test_fixture_rejects_unknown_models_without_echoing_input(self) -> None:
        secret = "do-not-echo-this-profile-secret"
        with ProviderFixture() as provider:
            status, body = request_json(
                f"{provider.base_url}/chat/completions",
                {"model": secret, "messages": []},
            )
            self.assertEqual(status, 400)
            self.assertNotIn(secret, json.dumps(body))
            self.assertEqual(provider.counters.snapshot()["invalid_requests"], 1)


if __name__ == "__main__":
    unittest.main()
