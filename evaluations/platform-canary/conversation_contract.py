"""Authenticated IC-008 conversation assertions for a platform canary."""

from __future__ import annotations

import time
import uuid
from collections.abc import Mapping

from conversation_http import ConversationContractError, ConversationHttpClient, component


MAX_ROUTE_SAMPLES = 64
MAX_STATUS_POLLS = 120
SHA256_PREFIX = "sha256:"


def _mapping(record: Mapping[str, object], name: str) -> Mapping[str, object]:
    value = record.get(name)
    if not isinstance(value, Mapping):
        raise ConversationContractError(f"{name} is missing from the receipt")
    return value


def _integer(value: object, label: str) -> int:
    if type(value) is not int or value < 0:
        raise ConversationContractError(f"{label} is invalid")
    return value


def _fingerprint(value: object, label: str) -> str:
    if not isinstance(value, str) or not value.startswith(SHA256_PREFIX):
        raise ConversationContractError(f"{label} is invalid")
    digest = value.removeprefix(SHA256_PREFIX)
    if len(digest) != 64 or any(character not in "0123456789abcdef" for character in digest):
        raise ConversationContractError(f"{label} is invalid")
    return value


def assert_status(record: Mapping[str, object], expected: int) -> None:
    if record.get("status") != expected:
        raise ConversationContractError("HTTP status did not match the canary contract")


def assert_receiver_no_store(
    record: Mapping[str, object], expected_receiver: str | None = None
) -> str:
    headers = _mapping(record, "headers")
    receiver = record.get("receiver")
    if (
        not isinstance(receiver, str)
        or not receiver
        or len(receiver.encode("utf-8")) > 256
        or not receiver.isprintable()
    ):
        raise ConversationContractError("response receiver is invalid")
    if expected_receiver is not None and receiver != expected_receiver:
        raise ConversationContractError("response reached an unexpected receiver")
    if headers.get("x-ironcrew-instance-id") != receiver:
        raise ConversationContractError("response receiver attribution is inconsistent")
    if headers.get("cache-control") != "no-store":
        raise ConversationContractError("response is missing Cache-Control: no-store")
    content_type = headers.get("content-type")
    if not isinstance(content_type, str) or not content_type.startswith("application/json"):
        raise ConversationContractError("response content type is invalid")
    return receiver


def assert_replay(record: Mapping[str, object], expected: bool) -> None:
    headers = _mapping(record, "headers")
    replayed = headers.get("idempotency-replayed")
    if replayed != ("true" if expected else None):
        raise ConversationContractError("idempotency replay attribution is invalid")


def execution_identity(record: Mapping[str, object]) -> dict[str, object]:
    body = _mapping(record, "response")
    incarnation = body.get("incarnation_id")
    try:
        parsed_incarnation = uuid.UUID(incarnation) if isinstance(incarnation, str) else None
    except ValueError:
        parsed_incarnation = None
    if parsed_incarnation is None or str(parsed_incarnation) != incarnation:
        raise ConversationContractError("conversation incarnation is invalid")
    return {
        "conversation_id": body.get("conversation_id"),
        "flow": body.get("flow"),
        "revision": _integer(body.get("revision"), "conversation revision"),
        "incarnation_id": incarnation,
        "source_fingerprint": _fingerprint(
            body.get("source_fingerprint"), "conversation source fingerprint"
        ),
        "definition_fingerprint": _fingerprint(
            body.get("definition_fingerprint"), "conversation definition fingerprint"
        ),
    }


class ConversationContractClient:
    """Exercise IC-008 endpoints and return sanitized evidence records."""

    def __init__(
        self,
        base_url: str,
        bearer_token: str,
        *,
        mock_base_url: str | None = None,
        timeout_seconds: float = 10,
    ) -> None:
        self._http = ConversationHttpClient(
            base_url, bearer_token,
            mock_base_url=mock_base_url, timeout_seconds=timeout_seconds,
        )

    def __repr__(self) -> str:
        return "ConversationContractClient(<redacted>)"

    def sample_route(
        self, samples: int, *, minimum_receivers: int = 1, pause_seconds: float = 0
    ) -> list[dict[str, object]]:
        if (
            type(samples) is not int
            or type(minimum_receivers) is not int
            or not 1 <= samples <= MAX_ROUTE_SAMPLES
            or not 1 <= minimum_receivers <= samples
            or not 0 <= pause_seconds <= 10
        ):
            raise ConversationContractError("route sampling bounds are invalid")
        records = []
        for index in range(samples):
            record = self._http.request("route_capabilities", "GET", "/capabilities")
            assert_status(record, 200)
            receiver = assert_receiver_no_store(record)
            if _mapping(record, "response").get("instance_id") != receiver:
                raise ConversationContractError("capability identity does not match its receiver")
            records.append(record)
            if pause_seconds and index + 1 < samples:
                time.sleep(pause_seconds)
        if len({record["receiver"] for record in records}) < minimum_receivers:
            raise ConversationContractError("route sampling did not reach enough receivers")
        return records

    def start(
        self, flow: str, conversation_id: str, agent: str, max_history: int, receiver: str
    ) -> dict[str, object]:
        if (
            not isinstance(agent, str)
            or not agent.strip()
            or type(max_history) is not int
            or max_history <= 0
        ):
            raise ConversationContractError("conversation start parameters are invalid")
        record = self._http.request(
            "conversation_start", "POST", self._path(flow, conversation_id) + "/start",
            payload={"agent": agent, "max_history": max_history},
        )
        assert_status(record, 200)
        assert_receiver_no_store(record, receiver)
        identity = execution_identity(record)
        body = _mapping(record, "response")
        expected_events = f"/flows/{flow}/conversations/{conversation_id}/events"
        if (
            identity["conversation_id"] != conversation_id
            or identity["flow"] != flow
            or body.get("agent") != agent
            or body.get("events_url") != expected_events
        ):
            raise ConversationContractError("conversation start identity is inconsistent")
        return record

    def message(
        self, flow: str, conversation_id: str, content: str, key: str, receiver: str,
        *, identity: Mapping[str, object], replayed: bool = False,
    ) -> dict[str, object]:
        if not isinstance(content, str) or not content.strip():
            raise ConversationContractError("conversation message content is invalid")
        record = self._http.request(
            "conversation_message", "POST", self._path(flow, conversation_id) + "/messages",
            payload={"content": content}, idempotency_key=key, secret_values=(content,),
        )
        assert_status(record, 200)
        assert_receiver_no_store(record, receiver)
        assert_replay(record, replayed)
        body = _mapping(record, "response")
        if (
            body.get("conversation_id") != conversation_id
            or body.get("incarnation_id") != identity.get("incarnation_id")
            or body.get("definition_fingerprint") != identity.get("definition_fingerprint")
            or _integer(body.get("revision"), "message revision")
            <= _integer(identity.get("revision"), "start revision")
        ):
            raise ConversationContractError("conversation message identity is inconsistent")
        return record

    def history(
        self, flow: str, conversation_id: str, receiver: str,
        *, identity: Mapping[str, object], minimum_revision: int,
    ) -> dict[str, object]:
        _integer(minimum_revision, "minimum history revision")
        record = self._http.request(
            "conversation_history", "GET", self._path(flow, conversation_id) + "/history"
        )
        assert_status(record, 200)
        assert_receiver_no_store(record, receiver)
        body = _mapping(record, "response")
        if (
            body.get("conversation_id") != conversation_id
            or body.get("flow") != flow
            or body.get("incarnation_id") != identity.get("incarnation_id")
            or body.get("source_fingerprint") != identity.get("source_fingerprint")
            or body.get("definition_fingerprint") != identity.get("definition_fingerprint")
            or _integer(body.get("revision"), "history revision") < minimum_revision
        ):
            raise ConversationContractError("conversation history identity is inconsistent")
        return record

    def delete(
        self, flow: str, conversation_id: str, receiver: str, *, expected_status: int = 200
    ) -> dict[str, object]:
        record = self._http.request(
            "conversation_delete", "DELETE", self._path(flow, conversation_id)
        )
        assert_status(record, expected_status)
        assert_receiver_no_store(record, receiver)
        if expected_status == 200 and _mapping(record, "response").get("deleted") != conversation_id:
            raise ConversationContractError("conversation delete receipt is inconsistent")
        return record

    def assert_shared_store_sse_conflict(
        self, flow: str, conversation_id: str, receiver: str
    ) -> dict[str, object]:
        record = self._http.request(
            "conversation_sse_conflict", "GET", self._path(flow, conversation_id) + "/events",
            extra_headers={"Last-Event-ID": "shared-store-probe"},
        )
        assert_status(record, 409)
        assert_receiver_no_store(record, receiver)
        return record

    def mock_counts(self) -> dict[str, object]:
        return self._mock("mock_counts", "GET", "/counts")

    def mock_status(self) -> dict[str, object]:
        return self._mock("mock_status", "GET", "/status")

    def mock_release(self) -> dict[str, object]:
        return self._mock("mock_release", "POST", "/release")

    def wait_until_mock_blocked(
        self, attempts: int, *, pause_seconds: float = 0.1
    ) -> dict[str, object]:
        if (
            type(attempts) is not int
            or not 1 <= attempts <= MAX_STATUS_POLLS
            or not 0 <= pause_seconds <= 10
        ):
            raise ConversationContractError("mock status polling bounds are invalid")
        for index in range(attempts):
            record = self.mock_status()
            body = _mapping(record, "response")
            if body.get("blocked") is True and _integer(
                body.get("blocked_requests"), "blocked request count"
            ) > 0:
                return record
            if pause_seconds and index + 1 < attempts:
                time.sleep(pause_seconds)
        raise ConversationContractError("mock provider did not report a blocked request")

    def _mock(self, operation: str, method: str, path: str) -> dict[str, object]:
        record = self._http.request(operation, method, path, mock=True)
        assert_status(record, 200)
        headers = _mapping(record, "headers")
        if headers.get("cache-control") != "no-store":
            raise ConversationContractError("mock response is missing Cache-Control: no-store")
        content_type = headers.get("content-type")
        if not isinstance(content_type, str) or not content_type.startswith("application/json"):
            raise ConversationContractError("mock response content type is invalid")
        return record

    @staticmethod
    def _path(flow: str, conversation_id: str) -> str:
        return (
            f"/flows/{component(flow, 'flow')}/conversations/"
            f"{component(conversation_id, 'conversation id')}"
        )
