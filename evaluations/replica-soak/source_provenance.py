"""Secret-free provenance for an intentionally dirty evaluation worktree."""

from __future__ import annotations

import hashlib
import json
import subprocess
from pathlib import Path
from typing import Any


MAX_CHANGED_PATHS = 2_000
MAX_PATH_BYTES = 4_096


def safe_binary_path(root: Path, binary: Path) -> tuple[str, str]:
    """Return a non-sensitive binary label plus its path scope."""
    root, binary = root.resolve(), binary.resolve()
    try:
        relative = binary.relative_to(root)
    except ValueError:
        return binary.name, "external_basename"
    return relative.as_posix(), "repository_relative"


def _git(root: Path, *args: str) -> bytes:
    result = subprocess.run(
        ["git", *args],
        cwd=root,
        check=True,
        capture_output=True,
        timeout=20,
    )
    return result.stdout


def _paths(raw: bytes) -> set[str]:
    paths: set[str] = set()
    for item in raw.split(b"\0"):
        if not item:
            continue
        if len(item) > MAX_PATH_BYTES:
            raise ValueError("changed worktree path exceeds the provenance limit")
        path = item.decode("utf-8", "strict")
        if path.startswith("/") or ".." in Path(path).parts:
            raise ValueError("git returned a non-relative worktree path")
        paths.add(path)
    if len(paths) > MAX_CHANGED_PATHS:
        raise ValueError("changed worktree path count exceeds the provenance limit")
    return paths


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def worktree_provenance(root: Path) -> dict[str, Any]:
    """Bind HEAD plus every tracked change and non-ignored untracked file."""
    root = root.resolve()
    revision = _git(root, "rev-parse", "HEAD").decode("ascii", "strict").strip()
    changed = _paths(_git(root, "diff", "--name-only", "-z", "HEAD", "--"))
    changed.update(_paths(_git(root, "ls-files", "--others", "--exclude-standard", "-z")))

    entries: list[dict[str, Any]] = []
    for relative in sorted(changed):
        path = root / relative
        if path.is_symlink():
            raise ValueError(f"changed worktree path is a symlink: {relative}")
        if not path.exists():
            entries.append({"path": relative, "state": "deleted"})
            continue
        if not path.is_file():
            raise ValueError(f"changed worktree path is not a regular file: {relative}")
        entries.append(
            {
                "path": relative,
                "state": "file",
                "bytes": path.stat().st_size,
                "sha256": _sha256(path),
            }
        )

    canonical = json.dumps(
        {"revision": revision, "entries": entries},
        sort_keys=True,
        separators=(",", ":"),
    ).encode()
    return {
        "revision": revision,
        "dirty": bool(entries),
        "changed_path_count": len(entries),
        "changed_paths": entries,
        "worktree_manifest_sha256": hashlib.sha256(canonical).hexdigest(),
        "manifest_encoding": "sorted compact JSON of revision plus changed_paths",
    }
