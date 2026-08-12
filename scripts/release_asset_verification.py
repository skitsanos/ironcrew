"""Bounded validation helpers for signed release-image assets."""

from __future__ import annotations

import hashlib
import re
from pathlib import Path

ASSET_SIZE_LIMITS = {
    "archive": 512 * 1024 * 1024,
    "receipt": 1024 * 1024,
    "checksum": 1024,
    "bundle": 2 * 1024 * 1024,
}


def release_asset_sizes(
    state: object, tag: str, expected: set[str], error_type: type[RuntimeError]
) -> dict[str, int]:
    if not isinstance(state, dict) or any(
        state.get(key) != value
        for key, value in {
            "isDraft": False,
            "isPrerelease": False,
            "tagName": tag,
        }.items()
    ):
        raise error_type("tag must identify an exact published stable GitHub release")
    assets = state.get("assets")
    if not isinstance(assets, list):
        raise error_type("GitHub release asset metadata was invalid")
    selected: dict[str, int] = {}
    for asset in assets:
        if not isinstance(asset, dict):
            raise error_type("GitHub release asset metadata was invalid")
        name, size = asset.get("name"), asset.get("size")
        if name in expected:
            if name in selected or not isinstance(size, int) or isinstance(size, bool):
                raise error_type("release image asset metadata was invalid")
            selected[name] = size
    if set(selected) != expected:
        raise error_type("release image asset set was incomplete")
    for name, size in selected.items():
        if name.endswith(".sha256"):
            kind = "checksum"
        elif name.endswith(".bundle"):
            kind = "bundle"
        else:
            kind = "receipt" if name.endswith(".json") else "archive"
        if size < 1 or size > ASSET_SIZE_LIMITS[kind]:
            raise error_type(f"release {kind} asset size was outside its limit")
    return selected


def check_checksum(
    path: Path, checksum_path: Path, error_type: type[RuntimeError]
) -> None:
    parts = checksum_path.read_text(encoding="utf-8").strip().split()
    if len(parts) != 2 or parts[1].lstrip("*") != path.name:
        raise error_type("release checksum file was malformed")
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    observed = digest.hexdigest()
    if not re.fullmatch(r"[0-9a-f]{64}", parts[0]) or observed != parts[0]:
        raise error_type("release asset checksum did not match")
