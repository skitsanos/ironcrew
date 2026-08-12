from __future__ import annotations

import json
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


sys.path.insert(0, str(Path(__file__).resolve().parent))

from conversation_contract import (  # noqa: E402
    ConversationContractClient,
    ConversationContractError,
    assert_receiver_no_store,
    execution_identity,
)
from conversation_receipt import ReceiptError, sanitize_receipt  # noqa: E402


TOKEN = "bearer-secret-must-not-appear"
KEY = "idempotency-secret-must-not-appear"
CONTENT = "message-secret-must-not-appear"
INCARNATION = "12345678-1234-4234-8234-123456789abc"
SOURCE = "sha256:" + "a" * 64
DEFINITION = "sha256:" + "b" * 64


class State:
    def __init__(self) -> None:
        self.capabilities = 0
        self.keys: set[str] = set()
        self.reject_delete = True
        self.status_calls = 0
        self.mock_received_auth = False


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

    def _send(
        self, status: int, value: object, *, receiver: str | None = "pod-b", replay: bool = False
    ) -> None:
        body = json.dumps(value, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        if receiver is not None:
            self.send_header("X-IronCrew-Instance-Id", receiver)
        if replay:
            self.send_header("Idempotency-Replayed", "true")
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)

    def _authorized(self) -> bool:
        if self.headers.get("Authorization") == f"Bearer {TOKEN}":
            return True
        self._send(401, {"error": TOKEN})
        return False

    def _mock(self) -> None:
        self.server.state.mock_received_auth |= self.headers.get("Authorization") is not None

    @staticmethod
    def _identity() -> dict[str, object]:
        return {
            "conversation_id": "session",
            "flow": "chat",
            "revision": 0,
            "incarnation_id": INCARNATION,
            "source_fingerprint": SOURCE,
            "definition_fingerprint": DEFINITION,
        }

    def do_GET(self) -> None:  # noqa: N802
        if self.path == "/counts":
            self._mock()
            self._send(200, {
                "chat_completions": 2, "effect_calls": 1,
                "final_responses": 1, "tool_call_responses": 1,
            }, receiver=None)
            return
        if self.path == "/status":
            self._mock()
            self.server.state.status_calls += 1
            blocked = self.server.state.status_calls > 1
            self._send(200, {
                "chat_completions": 2, "effect_calls": 1,
                "final_responses": 1, "tool_call_responses": 1,
                "blocked": blocked, "blocked_requests": int(blocked),
                "blocking_content_configured": True, "release_generation": 0,
            }, receiver=None)
            return
        if not self._authorized():
            return
        if self.path == "/capabilities":
            self.server.state.capabilities += 1
            receiver = "pod-a" if self.server.state.capabilities % 2 else "pod-b"
            self._send(200, {
                "instance_id": receiver, "process_start_id": f"start-{receiver}",
                "lifecycle_state": "accepting",
                "deployment": {"revision": "git:test", "artifact_fingerprint": SOURCE},
            }, receiver=receiver)
            return
        if self.path.endswith("/history"):
            self._send(200, {
                **self._identity(), "revision": 1, "turn_count": 1, "truncated": False,
                "messages": [{"role": "user", "content": CONTENT},
                             {"role": "assistant", "content": f"mock:{CONTENT}"}],
            })
            return
        if self.path.endswith("/events"):
            self._send(409, {"error": CONTENT})
            return
        self._send(404, {"error": "not found"})

    def do_POST(self) -> None:  # noqa: N802
        if self.path == "/release":
            self._mock()
            self._read()
            self._send(200, {"release_generation": 1, "released_requests": 1}, receiver=None)
            return
        if not self._authorized():
            return
        body = self._read()
        if self.path.endswith("/start"):
            self._send(200, {
                **self._identity(), "agent": "coordinator",
                "events_url": "/flows/chat/conversations/session/events",
            })
            return
        if self.path.endswith("/messages"):
            key = self.headers.get("Idempotency-Key", "")
            replayed = key in self.server.state.keys
            self.server.state.keys.add(key)
            self._send(200, {
                "conversation_id": "session", "turn_index": 0, "turn_count": 1,
                "revision": 1, "incarnation_id": INCARNATION,
                "definition_fingerprint": DEFINITION,
                "assistant": body.decode(errors="ignore"), "reasoning": CONTENT,
            }, replay=replayed)
            return
        self._send(404, {"error": "not found"})

    def do_DELETE(self) -> None:  # noqa: N802
        if not self._authorized():
            return
        if self.server.state.reject_delete:
            self._send(409, {"error": CONTENT})
        else:
            self._send(200, {"deleted": "session"})


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


class ConversationContractTests(unittest.TestCase):
    @staticmethod
    def client(server: Server) -> ConversationContractClient:
        return ConversationContractClient(
            server.base_url, TOKEN, mock_base_url=server.base_url, timeout_seconds=2
        )

    def test_route_conversation_replay_sse_delete_and_receipt(self) -> None:
        with Fixture() as server:
            client = self.client(server)
            records = client.sample_route(2, minimum_receivers=2)
            started = client.start("chat", "session", "coordinator", 20, "pod-b")
            identity = execution_identity(started)
            first = client.message(
                "chat", "session", CONTENT, KEY, "pod-b", identity=identity
            )
            replay = client.message(
                "chat", "session", CONTENT, KEY, "pod-b", identity=identity, replayed=True
            )
            history = client.history(
                "chat", "session", "pod-b", identity=identity, minimum_revision=1
            )
            sse = client.assert_shared_store_sse_conflict("chat", "session", "pod-b")
            busy = client.delete("chat", "session", "pod-b", expected_status=409)
            server.state.reject_delete = False
            deleted = client.delete("chat", "session", "pod-b")
            receipt = sanitize_receipt(
                [*records, started, first, replay, history, sse, busy, deleted],
                secrets=(TOKEN, KEY, CONTENT),
            )
            serialized = json.dumps(receipt, sort_keys=True)
            self.assertNotIn(TOKEN, serialized)
            self.assertNotIn(KEY, serialized)
            self.assertNotIn(CONTENT, serialized)
            self.assertNotIn('"assistant":', serialized)
            self.assertEqual(history["response"]["messages"], [{"role": "user"}, {"role": "assistant"}])
            self.assertEqual(repr(client), "ConversationContractClient(<redacted>)")

    def test_mock_status_wait_release_and_counts_are_unauthenticated(self) -> None:
        with Fixture() as server:
            client = self.client(server)
            self.assertEqual(client.mock_counts()["response"]["effect_calls"], 1)
            blocked = client.wait_until_mock_blocked(2, pause_seconds=0)
            self.assertTrue(blocked["response"]["blocked"])
            released = client.mock_release()
            self.assertEqual(released["response"]["released_requests"], 1)
            self.assertFalse(server.state.mock_received_auth)

    def test_bounds_and_boundary_fail_closed(self) -> None:
        with Fixture() as server:
            client = self.client(server)
            with self.assertRaisesRegex(ConversationContractError, "sampling bounds"):
                client.sample_route(2, minimum_receivers=3)
            with self.assertRaisesRegex(ConversationContractError, "conversation id"):
                client.start("chat", "../session", "coordinator", 20, "pod-b")
            with self.assertRaisesRegex(ConversationContractError, "Cache-Control"):
                assert_receiver_no_store({
                    "receiver": "pod-a",
                    "headers": {"x-ironcrew-instance-id": "pod-a", "content-type": "application/json"},
                })

    def test_receipt_projection_redacts_safe_fields_and_drops_unsafe_fields(self) -> None:
        receipt = sanitize_receipt([{
            "operation": "conversation_message",
            "status": 200,
            "response_bytes": 42,
            "receiver": "pod-a",
            "url": "https://must-not-appear.invalid",
            "headers": {"Authorization": TOKEN, "Cache-Control": "no-store"},
            "response": {
                "conversation_id": CONTENT,
                "assistant": CONTENT,
                "definition_fingerprint": DEFINITION,
            },
        }], secrets=(TOKEN, CONTENT))
        serialized = json.dumps(receipt, sort_keys=True)
        self.assertEqual(receipt["records"][0]["response"]["conversation_id"], "<redacted>")
        self.assertNotIn(TOKEN, serialized)
        self.assertNotIn(CONTENT, serialized)
        self.assertNotIn("Authorization", serialized)
        self.assertNotIn("must-not-appear", serialized)
        self.assertNotIn('"assistant":', serialized)
        with self.assertRaisesRegex(ReceiptError, "record count"):
            sanitize_receipt([])


if __name__ == "__main__":
    unittest.main()
