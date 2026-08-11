from __future__ import annotations

import json
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any


sys.path.insert(0, str(Path(__file__).resolve().parent))

from http_contract import ContractClient, ContractError  # noqa: E402


TOKEN = "bearer-secret-must-not-appear"
KEY = "idempotency-secret-must-not-appear"
BODY_SECRET = "request-body-secret-must-not-appear"
ANSWER_SECRET = "answer-secret-must-not-appear"
EVENT_SECRET = "event-secret-must-not-appear"


class State:
    def __init__(self) -> None:
        self.capabilities = 0
        self.runs: dict[str, bytes] = {}
        self.effect_calls = 1


class Server(ThreadingHTTPServer):
    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), Handler)
        self.state = State()

    @property
    def base_url(self) -> str:
        host, port = self.server_address[:2]
        return f"http://{host}:{port}"


class Handler(BaseHTTPRequestHandler):
    server: Server
    protocol_version = "HTTP/1.1"

    def log_message(self, _format: str, *_args: object) -> None:
        return

    def _read(self) -> bytes:
        return self.rfile.read(int(self.headers.get("Content-Length", "0")))

    def _send(self, status: int, value: object, receiver: str = "pod-b") -> None:
        body = json.dumps(value, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("Retry-After", "7")
        self.send_header("X-IronCrew-Instance-Id", receiver)
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)

    def _authorized(self) -> bool:
        if self.headers.get("Authorization") == f"Bearer {TOKEN}":
            return True
        self._send(401, {"error": "unauthorized"})
        return False

    def do_GET(self) -> None:  # noqa: N802
        if self.path == "/counts":
            self._send(200, {"chat_completions": 2, "effect_calls": self.server.state.effect_calls,
                             "final_responses": 1, "tool_call_responses": 1})
            return
        if not self._authorized():
            return
        if self.path == "/capabilities":
            self.server.state.capabilities += 1
            receiver = "pod-a" if self.server.state.capabilities % 2 else "pod-b"
            self._send(200, {
                "instance_id": receiver,
                "process_start_id": f"start-{receiver}",
                "lifecycle_state": "accepting",
                "multi_replica_control": False,
                "live_control": {"human_input": "shared_store_for_keyed_runs",
                                 "sse_replay": "shared_store",
                                 "run_abort": {"cross_instance": "keyed_store_if_supported"}},
                "deployment": {"revision": "git:test", "artifact_fingerprint": "sha256:abc",
                               "flow_fingerprint": "sha256:def", "config_fingerprint": "sha256:ghi",
                               "hitl_keyring_fingerprint": "sha256:jkl"},
                "unexpected_secret": BODY_SECRET,
            }, receiver)
            return
        if "/questions/" in self.path:
            self._send(200, {"run_id": "run-1", "status": "waiting_for_input",
                             "owner_instance_id": "pod-a", "control_scope": "shared_store",
                             "questions": [{"question_id": "question-1", "kind": "question",
                                            "prompt": BODY_SECRET, "choices": [BODY_SECRET]}]})
            return
        if "/events/" in self.path:
            cursor = self.headers.get("Last-Event-ID")
            if cursor == "malformed":
                self._send(400, {"code": "invalid_cursor", "error": BODY_SECRET})
                return
            frames = (
                [("run-1:3", "run_complete", EVENT_SECRET)]
                if cursor else
                [("run-1:1", "run_started", EVENT_SECRET),
                 ("run-1:2", "human_input_requested", EVENT_SECRET)]
            )
            body = b"".join(
                f"id: {item_id}\nevent: {event}\ndata: {data}\n\n".encode()
                for item_id, event, data in frames
            )
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-store, no-transform")
            self.send_header("X-Accel-Buffering", "no")
            self.send_header("X-IronCrew-Instance-Id", "pod-b")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(body)
            return
        self._send(404, {"code": "not_found"})

    def do_POST(self) -> None:  # noqa: N802
        if self.path == "/reset":
            self._read()
            self.server.state.effect_calls = 0
            self._send(200, {"chat_completions": 0, "effect_calls": 0,
                             "final_responses": 0, "tool_call_responses": 0})
            return
        if not self._authorized():
            return
        body = self._read()
        if self.path.endswith("/run"):
            key = self.headers.get("Idempotency-Key", "")
            previous = self.server.state.runs.get(key)
            if previous is not None and previous != body:
                self._send(409, {"code": "idempotency_conflict", "error": KEY + BODY_SECRET})
                return
            self.server.state.runs[key] = body
            replayed = previous is not None
            response = {"run_id": "run-1", "status": "started", "owner_instance_id": "pod-a",
                        "control_scope": "process", "echo": BODY_SECRET}
            encoded = json.dumps(response, separators=(",", ":")).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(encoded)))
            self.send_header("Cache-Control", "no-store")
            self.send_header("X-IronCrew-Instance-Id", "pod-b")
            if replayed:
                self.send_header("Idempotency-Replayed", "true")
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(encoded)
            return
        if "/answer/" in self.path:
            self._send(202, {"run_id": "run-1", "question_id": "question-1", "status": "queued",
                             "owner_instance_id": "pod-a", "control_scope": "shared_store",
                             "echo": ANSWER_SECRET})
            return
        if "/abort/" in self.path:
            self._send(200, {"run_id": "run-1", "status": "cancellation_requested",
                             "owner_instance_id": "pod-a", "control_scope": "shared_store",
                             "already_requested": False})
            return
        self._send(404, {"code": "not_found"})


class Fixture:
    def __enter__(self) -> Server:
        self.server = Server()
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        return self.server

    def __exit__(self, *_args: object) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


def serialized(value: object) -> str:
    return json.dumps(value, sort_keys=True)


class HttpContractTests(unittest.TestCase):
    def client(self, server: Server) -> ContractClient:
        return ContractClient(server.base_url, TOKEN, mock_base_url=server.base_url, timeout_seconds=2)

    def assert_no_secrets(self, value: object) -> None:
        text = serialized(value)
        for secret in [TOKEN, KEY, BODY_SECRET, ANSWER_SECRET, EVENT_SECRET]:
            self.assertNotIn(secret, text)

    def test_capability_sampling_is_bounded_and_captures_receivers(self) -> None:
        with Fixture() as server:
            client = self.client(server)
            records = client.sample_capabilities(2)
            self.assertEqual([item["receiver"] for item in records], ["pod-a", "pod-b"])
            self.assertEqual(records[0]["response"]["deployment"]["revision"], "git:test")
            self.assertEqual(records[0]["headers"]["retry-after"], "7")
            self.assertNotIn("unexpected_secret", records[0]["response"])
            self.assertEqual(repr(client), "ContractClient(<redacted>)")
            self.assert_no_secrets(records)
            with self.assertRaisesRegex(ContractError, "sampling bounds"):
                client.sample_capabilities(65)

    def test_run_start_replay_and_conflict_are_sanitized(self) -> None:
        with Fixture() as server:
            client = self.client(server)
            payload = {"input": BODY_SECRET}
            started = client.start_run("demo", payload, KEY)
            replayed = client.replay_run("demo", payload, KEY)
            conflict = client.conflict_run("demo", {"input": "different"}, KEY)
            self.assertEqual(started["status"], 200)
            self.assertEqual(replayed["headers"]["idempotency-replayed"], "true")
            self.assertEqual(conflict["status"], 409)
            self.assertEqual(conflict["response"], {"code": "idempotency_conflict"})
            self.assertEqual(client.start_run("demo", None, "null-key")["status"], 200)
            self.assertEqual(client.conflict_run("demo", {}, "null-key")["status"], 409)
            self.assert_no_secrets([started, replayed, conflict])

    def test_question_answer_abort_and_poll_bounds(self) -> None:
        with Fixture() as server:
            client = self.client(server)
            question = client.wait_for_question("demo", "run-1", 2)
            self.assertEqual(question["response"]["questions"],
                             [{"question_id": "question-1"}])
            answer = client.answer_question("demo", "run-1", "question-1", ANSWER_SECRET)
            abort = client.abort_run("demo", "run-1")
            self.assertEqual(answer["status"], 202)
            self.assertEqual(abort["response"]["status"], "cancellation_requested")
            self.assert_no_secrets([question, answer, abort])
            with self.assertRaisesRegex(ContractError, "polling bounds"):
                client.wait_for_question("demo", "run-1", 121)

    def test_sse_collection_reconnect_and_cursor_error_are_bounded(self) -> None:
        with Fixture() as server:
            client = self.client(server)
            initial = client.collect_sse("demo", "run-1", max_events=2, max_bytes=4096)
            self.assertEqual([frame["id"] for frame in initial["frames"]],
                             ["run-1:1", "run-1:2"])
            self.assertEqual(initial["last_event_id"], "run-1:2")
            resumed = client.reconnect_sse("demo", "run-1", "run-1:2", max_events=2)
            self.assertEqual(resumed["last_event_id"], "run-1:3")
            error = client.cursor_error("demo", "run-1", "malformed")
            self.assertEqual((error["status"], error["response"]),
                             (400, {"code": "invalid_cursor"}))
            self.assert_no_secrets([initial, resumed, error])
            with self.assertRaisesRegex(ContractError, "collection bounds"):
                client.collect_sse("demo", "run-1", max_events=257)

    def test_mock_counts_and_reset_do_not_send_authentication(self) -> None:
        with Fixture() as server:
            client = self.client(server)
            counts = client.mock_counts()
            reset = client.mock_reset()
            self.assertEqual(counts["status"], 200)
            self.assertEqual(reset["status"], 200)
            self.assertEqual(reset["response"]["effect_calls"], 0)
            self.assertEqual(counts["response"]["effect_calls"], 1)
            self.assert_no_secrets([counts, reset])


if __name__ == "__main__":
    unittest.main()
