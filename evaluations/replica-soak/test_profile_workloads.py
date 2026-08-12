import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


sys.path.insert(0, str(Path(__file__).resolve().parent))
from profile_contract import profile_metadata  # noqa: E402
from profile_provider import LARGE_RESULT_BYTES  # noqa: E402
from profile_runtime import child_environment, scan_logs  # noqa: E402
from profile_workloads import SHARED_STORE_SSE_ERROR, run_profile_suite  # noqa: E402


class Response:
    def __init__(self, status: int, body: dict[str, object]) -> None:
        self.status = status
        self.body = body

    def json(self) -> dict[str, object]:
        return self.body


class FakeClient:
    def __init__(self) -> None:
        self.conversation_id: str | None = None
        self.counts = {
            "provider_tool_provider_calls": 0,
            "provider_tool_tool_effects": 0,
            "large_result_provider_calls": 0,
            "conversation_provider_calls": 0,
            "invalid_requests": 0,
        }

    def request(
        self,
        operation: str,
        _method: str,
        _url: str,
        _payload: object | None = None,
        _headers: dict[str, str] | None = None,
    ) -> Response:
        if operation == "profile-provider-tool_start":
            self.counts["provider_tool_provider_calls"] += 2
            self.counts["provider_tool_tool_effects"] += 1
            return Response(200, {"run_id": "tool-run"})
        if operation == "profile-provider-tool_read":
            return self.run("ic018-provider-tool-ok")
        if operation == "profile-large-result_start":
            self.counts["large_result_provider_calls"] += 1
            return Response(200, {"run_id": "large-run"})
        if operation == "profile-large-result_read":
            return self.run("L" * LARGE_RESULT_BYTES)
        if operation == "conversation_start_owner":
            self.conversation_id = _url.removesuffix("/start").rsplit("/", 1)[-1]
            return Response(200, self.conversation(1))
        if operation == "conversation_message_warm_owner":
            self.counts["conversation_provider_calls"] += 1
            return Response(200, {**self.conversation(2), "assistant": "ic018-conversation-ok"})
        if operation == "conversation_message_cold_peer":
            self.counts["conversation_provider_calls"] += 1
            return Response(200, {**self.conversation(3), "assistant": "ic018-conversation-ok"})
        if operation == "conversation_history_owner":
            return Response(
                200,
                {
                    **self.conversation(3),
                    "turn_count": 2,
                    "messages": [
                        {"role": "system"},
                        {"role": "user"},
                        {"role": "assistant"},
                        {"role": "user"},
                        {"role": "assistant"},
                    ],
                },
            )
        if operation == "conversation_sse_shared_store_boundary":
            return Response(409, {"error": SHARED_STORE_SSE_ERROR})
        if operation == "conversation_delete_peer":
            conversation_id = _url.split("/")[-1]
            return Response(200, {"deleted": conversation_id})
        raise AssertionError(f"unexpected operation {operation}")

    @staticmethod
    def run(output: str) -> Response:
        return Response(
            200,
            {"status": "Success", "task_results": [{"output": output}]},
        )

    def conversation(self, revision: int) -> dict[str, object]:
        return {
            "conversation_id": self.conversation_id,
            "incarnation_id": "11111111-1111-4111-8111-111111111111",
            "definition_fingerprint": f"sha256:{'a' * 64}",
            "revision": revision,
        }


class RevisionDriftClient(FakeClient):
    def request(self, operation: str, *args: object, **kwargs: object) -> Response:
        response = super().request(operation, *args, **kwargs)
        if operation == "conversation_history_owner":
            response.body["revision"] = 4
        return response


class WrongSseConflictClient(FakeClient):
    def request(self, operation: str, *args: object, **kwargs: object) -> Response:
        response = super().request(operation, *args, **kwargs)
        if operation == "conversation_sse_shared_store_boundary":
            response.body["error"] = "Conversation is busy"
        return response


class ProfileWorkloadTests(unittest.TestCase):
    def test_suite_is_machine_readable_and_exactly_classified(self) -> None:
        client = FakeClient()
        results = run_profile_suite(
            ("http://replica-a", "http://replica-b"),
            client,
            lambda: dict(client.counts),
            poll_interval_seconds=0.01,
        )
        self.assertEqual([result["status"] for result in results], ["passed"] * 3)
        self.assertEqual(
            [result["provider_evidence_class"] for result in results],
            ["bounded_loopback_mock"] * 3,
        )
        self.assertTrue(all(result["actual_paid_provider_calls"] == 0 for result in results))
        conversation = results[-1]["evidence"]
        self.assertFalse(conversation["in_flight_takeover_proven"])
        self.assertEqual(conversation["steps"][2]["route"], "cold_peer_b_committed_boundary_rehydration")
        self.assertEqual(conversation["steps"][4]["status"], 409)
        self.assertNotIn("bounded warm-owner turn", json.dumps(results))

    def test_metadata_returns_independent_policy_copies(self) -> None:
        first = profile_metadata()
        first[0]["bounds"]["runs"] = 99
        self.assertEqual(profile_metadata()[0]["bounds"]["runs"], 1)
        self.assertTrue(all(not item["live_provider_evidence"] for item in profile_metadata()))

    def test_conversation_revision_drift_fails_only_that_profile(self) -> None:
        client = RevisionDriftClient()
        results = run_profile_suite(
            ("http://replica-a", "http://replica-b"),
            client,
            lambda: dict(client.counts),
            poll_interval_seconds=0.01,
        )
        self.assertEqual([result["status"] for result in results], ["passed", "passed", "failed"])
        self.assertEqual(results[-1]["error"]["kind"], "ConversationContractError")

    def test_unrelated_conversation_sse_conflict_is_not_accepted(self) -> None:
        client = WrongSseConflictClient()
        results = run_profile_suite(
            ("http://replica-a", "http://replica-b"),
            client,
            lambda: dict(client.counts),
            poll_interval_seconds=0.01,
        )
        self.assertEqual(results[-1]["status"], "failed")
        self.assertIn("shared-store boundary", results[-1]["error"]["message"])

    def test_child_environment_replaces_live_provider_inputs(self) -> None:
        parent = {
            "PATH": "/usr/bin",
            "HOME": "/safe-home",
            "OPENAI_API_KEY": "live-openai-secret",
            "OPENAI_BASE_URL": "https://paid.example/v1",
            "ANTHROPIC_API_KEY": "live-anthropic-secret",
            "SENSITIVE_OTHER": "not-inherited",
        }
        with patch.dict(os.environ, parent, clear=True):
            environment = child_environment(
                "postgres://user:password@db/profile",
                "ic018p_test_",
                "profile-token",
                "profile-a",
                "http://127.0.0.1:8123/v1",
                Path("/tmp/profile-output"),
            )
        serialized = json.dumps(environment)
        self.assertNotIn("live-openai-secret", serialized)
        self.assertNotIn("live-anthropic-secret", serialized)
        self.assertNotIn("paid.example", serialized)
        self.assertNotIn("not-inherited", serialized)
        self.assertEqual(environment["OPENAI_BASE_URL"], "http://127.0.0.1:8123/v1")
        self.assertEqual(environment["IRONCREW_ENV_ALLOWLIST"], "IC018_PROFILE_PROVIDER_BASE_URL")

    def test_log_scan_rejects_secret_canaries(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            logs = Path(directory)
            (logs / "replica-a.log").write_text("safe log", encoding="utf-8")
            self.assertEqual(scan_logs(logs, ("secret-canary",))["files"], {"replica-a.log": 8})
            (logs / "replica-b.log").write_text("secret-canary", encoding="utf-8")
            with self.assertRaisesRegex(RuntimeError, "secret-canary"):
                scan_logs(logs, ("secret-canary",))


if __name__ == "__main__":
    unittest.main()
