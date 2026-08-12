"""Identity and revision assertions for the IC-018 conversation profile."""

from __future__ import annotations

import uuid
from collections import Counter
from typing import Any


class ConversationContractError(RuntimeError):
    """The committed conversation boundary did not match the profile contract."""


def _fingerprint(value: object) -> bool:
    if not isinstance(value, str) or not value.startswith("sha256:"):
        return False
    digest = value.removeprefix("sha256:")
    return len(digest) == 64 and all(character in "0123456789abcdef" for character in digest)


def _canonical_uuid(value: object) -> bool:
    if not isinstance(value, str):
        return False
    try:
        return str(uuid.UUID(value)) == value
    except ValueError:
        return False


def validate_conversation_state(
    conversation_id: str,
    started: dict[str, Any],
    first: dict[str, Any],
    second: dict[str, Any],
    history: dict[str, Any],
) -> dict[str, Any]:
    identity = {
        "conversation_id": started.get("conversation_id"),
        "incarnation_id": started.get("incarnation_id"),
        "definition_fingerprint": started.get("definition_fingerprint"),
    }
    if (
        identity["conversation_id"] != conversation_id
        or not _canonical_uuid(identity["incarnation_id"])
        or not _fingerprint(identity["definition_fingerprint"])
        or any(
            response.get(name) != value
            for response in (first, second, history)
            for name, value in identity.items()
        )
    ):
        raise ConversationContractError(
            "conversation responses changed durable execution identity"
        )

    revisions = (
        started.get("revision"),
        first.get("revision"),
        second.get("revision"),
        history.get("revision"),
    )
    if (
        any(not isinstance(revision, int) or isinstance(revision, bool) for revision in revisions)
        or revisions[0] < 0
        or revisions[1] != revisions[0] + 1
        or revisions[2] != revisions[1] + 1
        or revisions[3] != revisions[2]
    ):
        raise ConversationContractError(
            f"conversation revisions did not advance across the cold peer: {revisions!r}"
        )

    messages = history.get("messages")
    roles = Counter(
        message.get("role") for message in messages if isinstance(message, dict)
    ) if isinstance(messages, list) else Counter()
    if history.get("turn_count") != 2 or roles != Counter(
        {"system": 1, "user": 2, "assistant": 2}
    ):
        raise ConversationContractError(
            "conversation history did not retain exactly two turns"
        )
    return {
        "identity": identity,
        "revisions": list(revisions),
        "roles": dict(sorted(roles.items())),
    }
