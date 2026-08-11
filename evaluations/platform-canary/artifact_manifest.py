#!/usr/bin/env python3
"""Create a deterministic, secret-blind IronCrew build attestation."""

from __future__ import annotations

import argparse
import fnmatch
import hashlib
import json
import os
import re
import stat
import subprocess
from pathlib import Path
from typing import Any

from fingerprints import FingerprintError, flow_tree_fingerprint


ROOT_FILES = ("Cargo.toml", "Cargo.lock", "Dockerfile", ".dockerignore")
ROOT_TREES = ("src", "examples", "tests")
PATHSPECS = (*ROOT_FILES, *ROOT_TREES)
FLOW_SCHEME = "ironcrew-platform-flow-tree-v1"
MAX_INPUT_FILES = 20_000
MAX_INPUT_FILE_BYTES = 64 * 1024 * 1024
MAX_INPUT_TOTAL_BYTES = 512 * 1024 * 1024
MAX_BINARY_BYTES = 1024 * 1024 * 1024
MAX_DOCKERIGNORE_BYTES = 64 * 1024
SHA256 = re.compile(r"sha256:[0-9a-f]{64}\Z")
COMMIT = re.compile(r"(?:[0-9a-f]{40}|[0-9a-f]{64})\Z")


class ManifestError(ValueError):
    """A fixed-message build-attestation validation failure."""

def _real_directory(path: Path, label: str) -> None:
    try:
        metadata = path.lstat()
    except OSError as error:
        raise ManifestError(f"{label} is unavailable") from error
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
        raise ManifestError(f"{label} must be a real directory")

def _read_regular(
    path: Path,
    label: str,
    max_bytes: int,
    *,
    retain: bool = False,
) -> tuple[int, str, bytes | None]:
    try:
        before = path.lstat()
    except OSError as error:
        raise ManifestError(f"{label} is unavailable") from error
    if stat.S_ISLNK(before.st_mode) or not stat.S_ISREG(before.st_mode):
        raise ManifestError(f"{label} must be a regular file")
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise ManifestError(f"{label} cannot be opened safely") from error
    digest = hashlib.sha256()
    chunks: list[bytes] | None = [] if retain else None
    observed = 0
    with os.fdopen(descriptor, "rb") as source:
        opened = os.fstat(source.fileno())
        if not stat.S_ISREG(opened.st_mode):
            raise ManifestError(f"{label} must be a regular file")
        if opened.st_size > max_bytes:
            raise ManifestError(f"{label} exceeds its byte limit")
        while chunk := source.read(1024 * 1024):
            observed += len(chunk)
            if observed > max_bytes:
                raise ManifestError(f"{label} exceeds its byte limit")
            digest.update(chunk)
            if chunks is not None:
                chunks.append(chunk)
        after = os.fstat(source.fileno())
    if observed != opened.st_size or (
        opened.st_size != after.st_size or opened.st_mtime_ns != after.st_mtime_ns
    ):
        raise ManifestError(f"{label} changed while hashing")
    body = b"".join(chunks) if chunks is not None else None
    return observed, f"sha256:{digest.hexdigest()}", body

def _parse_dockerignore(source: bytes) -> tuple[tuple[str, bool], ...]:
    try:
        text = source.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ManifestError(".dockerignore must be UTF-8") from error
    patterns: list[tuple[str, bool]] = []
    for raw in text.splitlines():
        rule = raw.strip()
        if not rule or rule.startswith("#"):
            continue
        if rule.startswith("!"):
            raise ManifestError(".dockerignore negation rules are unsupported")
        if (
            "\\" in rule
            or ".." in Path(rule).parts
            or "**" in rule
            or any(c in rule for c in "?[]")
        ):
            raise ManifestError(".dockerignore contains an unsupported rule")
        rule = rule.strip("/")
        if not rule:
            continue
        has_slash = "/" in rule
        if has_slash and "*" in rule:
            raise ManifestError(".dockerignore contains an unsupported rule")
        patterns.append((rule, has_slash))
    return tuple(patterns)

def _ignored(relative: str, patterns: tuple[tuple[str, bool], ...]) -> bool:
    components = relative.split("/")
    for pattern, has_slash in patterns:
        if has_slash:
            if relative == pattern or relative.startswith(f"{pattern}/"):
                return True
        elif any(fnmatch.fnmatchcase(component, pattern) for component in components):
            return True
    return False

def _relative(path: Path, root: Path) -> str:
    value = path.relative_to(root).as_posix()
    try:
        value.encode("utf-8")
    except UnicodeEncodeError as error:
        raise ManifestError("build input path must be valid UTF-8") from error
    return value

def _inventory(
    repository: Path,
) -> tuple[list[dict[str, Any]], tuple[tuple[str, bool], ...]]:
    records: list[dict[str, Any]] = []
    total = 0
    dockerignore: bytes | None = None
    for name in ROOT_FILES:
        size, digest, body = _read_regular(
            repository / name,
            "required build input",
            MAX_DOCKERIGNORE_BYTES if name == ".dockerignore" else MAX_INPUT_FILE_BYTES,
            retain=name == ".dockerignore",
        )
        total += size
        records.append({"path": name, "size": size, "sha256": digest})
        if body is not None:
            dockerignore = body
    assert dockerignore is not None
    patterns = _parse_dockerignore(dockerignore)

    for tree_name in ROOT_TREES:
        tree = repository / tree_name
        _real_directory(tree, "Docker input tree")
        pending = [tree]
        while pending:
            directory = pending.pop()
            try:
                entries = list(os.scandir(directory))
            except OSError as error:
                raise ManifestError("Docker input tree cannot be scanned") from error
            for entry in entries:
                path = Path(entry.path)
                relative = _relative(path, repository)
                if _ignored(relative, patterns):
                    continue
                if entry.is_symlink():
                    raise ManifestError("Docker input tree must not contain symlinks")
                if entry.is_dir(follow_symlinks=False):
                    pending.append(path)
                    continue
                if not entry.is_file(follow_symlinks=False):
                    raise ManifestError("Docker input tree must contain only regular files")
                size, digest, _ = _read_regular(path, "Docker build input", MAX_INPUT_FILE_BYTES)
                total += size
                if total > MAX_INPUT_TOTAL_BYTES:
                    raise ManifestError("Docker build inputs exceed the aggregate byte limit")
                records.append({"path": relative, "size": size, "sha256": digest})
                if len(records) > MAX_INPUT_FILES:
                    raise ManifestError("Docker build inputs exceed the file-count limit")
    records.sort(key=lambda item: item["path"].encode("utf-8"))
    return records, patterns

def _git(repository: Path, *arguments: str) -> bytes:
    process = subprocess.run(
        ["git", "-C", os.fspath(repository), *arguments],
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    if process.returncode != 0:
        raise ManifestError("Git source identity is unavailable")
    return process.stdout

def _base_commit(repository: Path) -> str:
    value = _git(repository, "rev-parse", "--verify", "HEAD^{commit}")
    value = value.strip().decode("ascii")
    if not COMMIT.fullmatch(value):
        raise ManifestError("Git base commit is not canonical")
    return value

def _is_build_path(path: str, patterns: tuple[tuple[str, bool], ...]) -> bool:
    in_tree = any(path.startswith(f"{tree}/") for tree in ROOT_TREES)
    return path in ROOT_FILES or (in_tree and not _ignored(path, patterns))

def _dirty(repository: Path, patterns: tuple[tuple[str, bool], ...]) -> bool:
    commands = (
        ("diff", "--name-only", "-z", "HEAD", "--", *PATHSPECS),
        ("ls-files", "--others", "-z", "--", *PATHSPECS),
    )
    for command in commands:
        for raw in _git(repository, *command).split(b"\0"):
            if not raw:
                continue
            try:
                path = raw.decode("utf-8")
            except UnicodeDecodeError as error:
                raise ManifestError("Git build-input path must be valid UTF-8") from error
            if _is_build_path(path, patterns):
                return True
    return False

def _reject_forbidden(repository: Path, candidate: Path, label: str) -> Path:
    absolute = Path(os.path.abspath(candidate))
    try:
        relative = absolute.relative_to(repository)
    except ValueError:
        return absolute
    if relative.parts and relative.parts[0] in {"docs", "target"}:
        raise ManifestError(f"{label} must not be read from an excluded repository tree")
    if any(part in {".env", ".env.build"} for part in relative.parts):
        raise ManifestError(f"{label} must not be read from an environment file")
    return absolute

def canonical_json(value: Any) -> bytes:
    """Serialize this schema as sorted, compact UTF-8 JSON without a newline."""
    return json.dumps(
        value, sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False
    ).encode("utf-8")


def create_receipt(
    repository: Path,
    binary: Path,
    flow_root: Path,
    supplied_flow_fingerprint: str,
) -> dict[str, Any]:
    repository = Path(os.path.abspath(repository))
    _real_directory(repository, "repository root")
    binary = _reject_forbidden(repository, binary, "binary")
    flow_root = _reject_forbidden(repository, flow_root, "flow root")
    if not SHA256.fullmatch(supplied_flow_fingerprint):
        raise ManifestError("supplied flow fingerprint must be canonical sha256")

    base_commit = _base_commit(repository)
    inputs, patterns = _inventory(repository)
    binary_size, binary_fingerprint, _ = _read_regular(binary, "runtime binary", MAX_BINARY_BYTES)
    try:
        verified_flow = flow_tree_fingerprint(flow_root)
    except FingerprintError as error:
        raise ManifestError(str(error)) from error
    if verified_flow != supplied_flow_fingerprint:
        raise ManifestError("supplied flow fingerprint does not match the flow tree")
    dirty = _dirty(repository, patterns)
    if _base_commit(repository) != base_commit:
        raise ManifestError("Git base commit changed while building the manifest")

    manifest = {
        "artifact": {"binary_fingerprint": binary_fingerprint, "size": binary_size},
        "flow": {
            "scheme": FLOW_SCHEME,
            "supplied_fingerprint": supplied_flow_fingerprint,
            "verified": True,
            "verified_fingerprint": verified_flow,
        },
        "schema": "ironcrew-build-attestation-v1",
        "source": {"base_commit": base_commit, "build_inputs": inputs, "dirty": dirty},
    }
    fingerprint = f"sha256:{hashlib.sha256(canonical_json(manifest)).hexdigest()}"
    return {"manifest": manifest, "manifest_sha256": fingerprint}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", type=Path, default=Path.cwd())
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--flow-root", type=Path, required=True)
    parser.add_argument("--flow-fingerprint", required=True)
    args = parser.parse_args()
    try:
        receipt = create_receipt(args.repository, args.binary, args.flow_root, args.flow_fingerprint)
    except ManifestError as error:
        parser.error(str(error))
    print(canonical_json(receipt).decode("utf-8"))


if __name__ == "__main__":
    main()
