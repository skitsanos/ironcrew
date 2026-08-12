"""Secret-free source provenance for a crew-effectiveness evaluation run."""

from __future__ import annotations

import hashlib
import json
import os
import stat
import subprocess
from pathlib import Path
from typing import Any


MAX_CHANGED_PATHS = 2_000
MAX_PATH_BYTES = 4_096
MAX_CHANGED_FILE_BYTES = 128 * 1024 * 1024
MAX_CHANGED_TOTAL_BYTES = 512 * 1024 * 1024
_HASH_CHUNK_BYTES = 1024 * 1024


def safe_binary_path(root: Path, binary: Path) -> tuple[str, str]:
    """Return a repository-relative label, or only an external basename."""
    if binary.is_symlink():
        raise ValueError("evaluation binary path must not be a symlink")
    root = root.resolve(strict=True)
    binary = binary.resolve(strict=True)
    if not binary.is_file():
        raise ValueError("evaluation binary path is not a regular file")
    try:
        label = binary.relative_to(root).as_posix()
        scope = "repository_relative"
    except ValueError:
        label = binary.name
        scope = "external_basename"
    if not label or len(label.encode("utf-8")) > MAX_PATH_BYTES:
        raise ValueError("evaluation binary label exceeds the provenance limit")
    return label, scope


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


def _tracked_diff(root: Path) -> bytes:
    return _git(
        root,
        "diff",
        "--binary",
        "--full-index",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--no-color",
        "HEAD",
        "--",
    )


def _file_entry(root: Path, relative: str, source: str) -> dict[str, Any]:
    path = root / relative
    try:
        before = path.lstat()
    except FileNotFoundError:
        if source == "tracked":
            return {"path": relative, "source": source, "state": "deleted"}
        raise ValueError(f"untracked worktree path disappeared: {relative}") from None
    if stat.S_ISLNK(before.st_mode):
        raise ValueError(f"changed worktree path is a symlink: {relative}")
    if not stat.S_ISREG(before.st_mode):
        raise ValueError(f"changed worktree path is not a regular file: {relative}")
    if before.st_size > MAX_CHANGED_FILE_BYTES:
        raise ValueError(f"changed worktree file exceeds the provenance limit: {relative}")

    flags = os.O_RDONLY | getattr(os, "O_BINARY", 0) | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise ValueError(f"could not safely open changed worktree path: {relative}") from error
    try:
        opened = os.fstat(descriptor)
        if not stat.S_ISREG(opened.st_mode) or not os.path.samestat(before, opened):
            raise ValueError(f"changed worktree path changed while captured: {relative}")
        digest = hashlib.sha256()
        while chunk := os.read(descriptor, _HASH_CHUNK_BYTES):
            digest.update(chunk)
        after = os.fstat(descriptor)
    finally:
        os.close(descriptor)
    if (opened.st_size, opened.st_mtime_ns) != (after.st_size, after.st_mtime_ns):
        raise ValueError(f"changed worktree path changed while captured: {relative}")
    return {
        "path": relative,
        "source": source,
        "state": "file",
        "bytes": after.st_size,
        "mode": stat.S_IMODE(after.st_mode),
        "sha256": digest.hexdigest(),
    }


def worktree_provenance(root: Path) -> dict[str, Any]:
    """Bind HEAD, every tracked diff byte, and every non-ignored untracked file."""
    root = root.resolve(strict=True)
    revision = _git(root, "rev-parse", "HEAD").decode("ascii", "strict").strip()
    tracked_diff = _tracked_diff(root)
    tracked = _paths(_git(root, "diff", "--name-only", "--no-renames", "-z", "HEAD", "--"))
    untracked = _paths(_git(root, "ls-files", "--others", "--exclude-standard", "-z"))
    if tracked & untracked:
        raise ValueError("git returned a path as both tracked and untracked")
    selected = tracked | untracked
    if len(selected) > MAX_CHANGED_PATHS:
        raise ValueError("changed worktree path count exceeds the provenance limit")

    entries = [
        _file_entry(root, relative, "tracked" if relative in tracked else "untracked")
        for relative in sorted(selected)
    ]
    total_bytes = sum(entry.get("bytes", 0) for entry in entries)
    if total_bytes > MAX_CHANGED_TOTAL_BYTES:
        raise ValueError("changed worktree bytes exceed the provenance limit")

    # Fail closed if HEAD, the tracked patch, or the selected path set changed
    # while file contents were being hashed.
    revision_after = _git(root, "rev-parse", "HEAD").decode("ascii", "strict").strip()
    tracked_after = _paths(
        _git(root, "diff", "--name-only", "--no-renames", "-z", "HEAD", "--")
    )
    untracked_after = _paths(_git(root, "ls-files", "--others", "--exclude-standard", "-z"))
    tracked_diff_after = _tracked_diff(root)
    if (
        revision != revision_after
        or tracked != tracked_after
        or untracked != untracked_after
        or tracked_diff != tracked_diff_after
    ):
        raise ValueError("worktree changed while provenance was captured")

    tracked_diff_sha256 = hashlib.sha256(tracked_diff).hexdigest()
    canonical = json.dumps(
        {
            "revision": revision,
            "tracked_diff_sha256": tracked_diff_sha256,
            "changed_paths": entries,
        },
        sort_keys=True,
        separators=(",", ":"),
    ).encode()
    return {
        "revision": revision,
        "dirty": bool(entries),
        "tracked_changed_path_count": len(tracked),
        "untracked_path_count": len(untracked),
        "changed_path_count": len(entries),
        "changed_file_bytes": total_bytes,
        "changed_paths": entries,
        "tracked_diff_sha256": tracked_diff_sha256,
        "worktree_manifest_sha256": hashlib.sha256(canonical).hexdigest(),
        "manifest_encoding": (
            "sorted compact JSON of revision, tracked_diff_sha256, and changed_paths"
        ),
    }


def require_unchanged_provenance(start: dict[str, Any], end: dict[str, Any]) -> None:
    """Reject a run whose source snapshot changed after its start receipt."""
    if start != end:
        raise ValueError("evaluation worktree provenance changed during execution")
