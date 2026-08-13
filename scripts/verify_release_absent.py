#!/usr/bin/env python3
"""Fail closed unless GitHub definitively reports that a release is absent."""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import urllib.error
import urllib.parse
import urllib.request

REPOSITORY = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
TAG = re.compile(r"^v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)$")


class ReleasePresenceError(RuntimeError):
    pass


def require_absent(repository: str, tag: str, token: str) -> None:
    if not REPOSITORY.fullmatch(repository):
        raise ReleasePresenceError("repository must use the owner/name form")
    if not TAG.fullmatch(tag):
        raise ReleasePresenceError("tag must use the stable vX.Y.Z form")
    if not token:
        raise ReleasePresenceError("GH_TOKEN is required")
    encoded_tag = urllib.parse.quote(tag, safe="")
    request = urllib.request.Request(
        f"https://api.github.com/repos/{repository}/releases/tags/{encoded_tag}",
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {token}",
            "User-Agent": "ironcrew-release-guard",
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            body = response.read(64 * 1024)
            if response.status != 200:
                raise ReleasePresenceError(
                    f"GitHub returned unexpected release lookup status {response.status}"
                )
            try:
                document = json.loads(body)
            except json.JSONDecodeError as error:
                raise ReleasePresenceError("GitHub returned invalid release metadata") from error
            if document.get("tag_name") != tag:
                raise ReleasePresenceError("GitHub returned mismatched release metadata")
            raise ReleasePresenceError(
                f"release {tag} already exists; signed assets are immutable"
            )
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return
        raise ReleasePresenceError(
            f"GitHub release lookup failed closed with HTTP {error.code}"
        ) from error
    except urllib.error.URLError as error:
        raise ReleasePresenceError("GitHub release lookup failed closed") from error


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--tag", required=True)
    arguments = parser.parse_args()
    try:
        require_absent(arguments.repository, arguments.tag, os.environ.get("GH_TOKEN", ""))
    except ReleasePresenceError as error:
        print(f"release absence guard: {error}", file=sys.stderr)
        return 1
    print(f"Release absence verified for {arguments.tag}.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
