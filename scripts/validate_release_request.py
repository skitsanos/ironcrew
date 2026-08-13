#!/usr/bin/env python3
"""Validate a sole-owner issue request for a privileged release dispatch."""

from __future__ import annotations

import argparse
import json
import re
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any

MAX_EVENT_BYTES = 256 * 1024
MAX_BODY_BYTES = 512
REQUEST_LABEL = "release-request"
REQUEST_TITLE = "IronCrew release request"
TARGETS = {"release", "docker"}
MODES = {"publish", "validate"}
TAG = re.compile(r"^v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)$")
REPOSITORY = re.compile(r"^[A-Za-z0-9][A-Za-z0-9-]*/[A-Za-z0-9._-]+$")
ACTOR = re.compile(r"^[A-Za-z0-9](?:[A-Za-z0-9-]*|[A-Za-z0-9-]*\[bot\])$")


class RequestError(ValueError):
    pass


@dataclass(frozen=True)
class Request:
    relevant: bool
    target: str = ""
    tag: str = ""
    mode: str = ""


def load_event(path: Path) -> Any:
    if not path.is_file():
        raise RequestError("event payload is missing")
    if path.stat().st_size > MAX_EVENT_BYTES:
        raise RequestError(f"event payload exceeds {MAX_EVENT_BYTES} bytes")
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise RequestError("event payload is not valid UTF-8 JSON") from error


def _object_without_duplicates(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise RequestError("request body contains a duplicate field")
        result[key] = value
    return result


def _canonical_request(body: Any) -> dict[str, Any]:
    if not isinstance(body, str):
        raise RequestError(f"request body must be at most {MAX_BODY_BYTES} UTF-8 bytes")
    try:
        body_bytes = body.encode("utf-8")
    except UnicodeEncodeError as error:
        raise RequestError("request body is not valid UTF-8") from error
    if len(body_bytes) > MAX_BODY_BYTES:
        raise RequestError(f"request body must be at most {MAX_BODY_BYTES} UTF-8 bytes")
    try:
        request = json.loads(body, object_pairs_hook=_object_without_duplicates)
    except (UnicodeError, json.JSONDecodeError) as error:
        raise RequestError("request body is not valid JSON") from error
    if not isinstance(request, dict) or list(request) != ["target", "tag", "mode"]:
        raise RequestError("request body must contain target, tag, and mode in canonical order")
    canonical = json.dumps(request, ensure_ascii=True, separators=(",", ":"))
    if body != canonical:
        raise RequestError("request body must be canonical single-line JSON")
    return request


def validate(
    event: Any,
    expected_repository: str,
    expected_actor: str,
    expected_triggering_actor: str,
    expected_owner: str,
) -> Request:
    if not REPOSITORY.fullmatch(expected_repository):
        raise RequestError("expected repository identity is invalid")
    if not ACTOR.fullmatch(expected_owner) or len(expected_owner) > 100:
        raise RequestError("expected owner identity is invalid")
    if not isinstance(event, dict) or event.get("action") != "labeled":
        raise RequestError("event must be an issues:labeled event")
    repository = event.get("repository")
    owner = repository.get("owner") if isinstance(repository, dict) else None
    if (
        not isinstance(repository, dict)
        or repository.get("full_name") != expected_repository
        or repository.get("default_branch") != "main"
        or not isinstance(owner, dict)
        or owner.get("login") != expected_owner
    ):
        raise RequestError("repository identity, owner, and default branch must match")
    label = event.get("label")
    if not isinstance(label, dict) or not isinstance(label.get("name"), str):
        raise RequestError("labeled event is missing its label identity")
    if label["name"] != REQUEST_LABEL:
        return Request(relevant=False)

    for name, identity in (
        ("actor", expected_actor),
        ("triggering actor", expected_triggering_actor),
    ):
        if not ACTOR.fullmatch(identity) or len(identity) > 100:
            raise RequestError(f"expected {name} identity is invalid")
        if identity != expected_owner:
            raise RequestError("only the configured owner may request or rerun a dispatch")
    sender = event.get("sender")
    if not isinstance(sender, dict) or sender.get("login") != expected_actor:
        raise RequestError("event sender did not match the triggering owner")

    issue = event.get("issue")
    if not isinstance(issue, dict) or "pull_request" in issue:
        raise RequestError("request must be a repository issue, not a pull request")
    author = issue.get("user")
    labels = issue.get("labels")
    if (
        issue.get("state") != "open"
        or issue.get("title") != REQUEST_TITLE
        or issue.get("author_association") != "OWNER"
        or not isinstance(author, dict)
        or author.get("login") != expected_owner
        or not isinstance(issue.get("number"), int)
        or isinstance(issue.get("number"), bool)
        or issue["number"] <= 0
    ):
        raise RequestError("request issue must be open and owned with the exact title")
    if (
        not isinstance(labels, list)
        or len(labels) != 1
        or not isinstance(labels[0], dict)
        or labels[0].get("name") != REQUEST_LABEL
    ):
        raise RequestError("request issue must have exactly the release-request label")

    request = _canonical_request(issue.get("body"))
    target, tag, mode = request["target"], request["tag"], request["mode"]
    if not isinstance(target, str) or target not in TARGETS:
        raise RequestError("target must be release or docker")
    if not isinstance(tag, str) or not TAG.fullmatch(tag):
        raise RequestError("tag must be an exact stable tag in vX.Y.Z form")
    if not isinstance(mode, str) or mode not in MODES:
        raise RequestError("mode must be publish or validate")
    return Request(relevant=True, target=target, tag=tag, mode=mode)


def write_outputs(path: Path, request: Request) -> None:
    with path.open("a", encoding="utf-8") as output:
        output.write(f"relevant={'true' if request.relevant else 'false'}\n")
        if request.relevant:
            output.write(
                f"target={request.target}\ntag={request.tag}\nmode={request.mode}\n"
            )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--event", required=True, type=Path)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--actor", required=True)
    parser.add_argument("--triggering-actor", required=True)
    parser.add_argument("--owner", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    try:
        request = validate(
            load_event(args.event),
            args.repository,
            args.actor,
            args.triggering_actor,
            args.owner,
        )
        write_outputs(args.output, request)
    except (OSError, RequestError) as error:
        print(f"release request validation: {error}", file=sys.stderr)
        return 1
    if request.relevant:
        print(
            f"Validated owner request for {request.target} {request.tag} "
            f"in {request.mode} mode."
        )
    else:
        print("Ignoring unrelated issue label.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
