#!/usr/bin/env python3
"""Validate the exact payload accepted by privileged repository dispatches."""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path
from typing import Any

MAX_EVENT_BYTES = 128 * 1024
TAG = re.compile(r"^v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)$")
REPOSITORY = re.compile(r"^[A-Za-z0-9][A-Za-z0-9-]*/[A-Za-z0-9._-]+$")
ACTOR = re.compile(r"^[A-Za-z0-9](?:[A-Za-z0-9-]*|[A-Za-z0-9-]*\[bot\])$")
EVENT_TYPES = {"ironcrew_release_v1", "ironcrew_docker_publish_v1"}
MODES = {"publish", "validate"}


class DispatchError(ValueError):
    pass


def load_event(path: Path) -> Any:
    if not path.is_file():
        raise DispatchError("event payload is missing")
    if path.stat().st_size > MAX_EVENT_BYTES:
        raise DispatchError(f"event payload exceeds {MAX_EVENT_BYTES} bytes")
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise DispatchError("event payload is not valid UTF-8 JSON") from error


def validate(
    event: Any,
    expected_type: str,
    expected_repository: str,
    expected_actor: str,
) -> tuple[str, str]:
    if expected_type not in EVENT_TYPES:
        raise DispatchError("unsupported repository dispatch type")
    if not REPOSITORY.fullmatch(expected_repository):
        raise DispatchError("expected repository identity is invalid")
    if not ACTOR.fullmatch(expected_actor) or len(expected_actor) > 100:
        raise DispatchError("expected sender identity is invalid")
    if not isinstance(event, dict) or event.get("action") != expected_type:
        raise DispatchError("repository dispatch type did not match the workflow")
    repository = event.get("repository")
    if (
        not isinstance(repository, dict)
        or repository.get("full_name") != expected_repository
        or repository.get("default_branch") != "main"
    ):
        raise DispatchError("repository identity and default branch must match trusted context")
    sender = event.get("sender")
    if not isinstance(sender, dict) or sender.get("login") != expected_actor:
        raise DispatchError("event sender did not match the triggering actor")
    payload = event.get("client_payload")
    if not isinstance(payload, dict) or set(payload) != {"mode", "tag"}:
        raise DispatchError("client_payload must contain exactly mode and tag")
    mode, tag = payload["mode"], payload["tag"]
    if not isinstance(mode, str) or mode not in MODES:
        raise DispatchError("mode must be publish or validate")
    if not isinstance(tag, str) or not TAG.fullmatch(tag):
        raise DispatchError("tag must be an exact stable tag in vX.Y.Z form")
    return tag, mode


def write_outputs(path: Path, tag: str, mode: str) -> None:
    with path.open("a", encoding="utf-8") as output:
        output.write(f"tag={tag}\nmode={mode}\n")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--event", required=True, type=Path)
    parser.add_argument("--event-type", required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--actor", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    try:
        tag, mode = validate(
            load_event(args.event), args.event_type, args.repository, args.actor
        )
        write_outputs(args.output, tag, mode)
    except (DispatchError, OSError) as error:
        print(f"release dispatch validation: {error}", file=sys.stderr)
        return 1
    print(f"Validated {args.event_type} request for {tag} in {mode} mode.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
