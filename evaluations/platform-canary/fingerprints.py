#!/usr/bin/env python3
"""Canonical, secret-safe fingerprints for the IC-007 platform canary."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import re
import stat
from collections.abc import Mapping, Sequence
from pathlib import Path

from config_contract import (
    CONFIG_ENV_ALLOWLIST,
    DERIVED_PRESENCE_NAMES,
    FORBIDDEN_CONFIG_NAMES,
    OPTIONAL_CONFIG_ENV_NAMES,
    SENSITIVE_NAME_FRAGMENTS,
    UNHASHED_IRONCREW_ENV_NAMES,
)


FLOW_DOMAIN = b"ironcrew-platform-flow-tree-v1"
CONFIG_DOMAIN = b"ironcrew-platform-effective-config-v1"
KEYRING_DOMAIN = b"ironcrew-platform-hitl-keyring-v1"
MAX_FLOW_FILES = 4096
MAX_FLOW_FILE_BYTES = 16 * 1024 * 1024
MAX_FLOW_TOTAL_BYTES = 64 * 1024 * 1024
MAX_CONFIG_VALUE_BYTES = 4096
MAX_KEYRING_JSON_BYTES = 16 * 1024
MAX_KEYS = 8
KEY_BYTES = 32
KEY_ID = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,63}\Z")
ENV_NAME = re.compile(r"[A-Z][A-Z0-9_]*\Z")

class FingerprintError(ValueError):
    """A fixed-message fingerprint validation failure."""


def _frame(digest: "hashlib._Hash", value: bytes) -> None:
    digest.update(len(value).to_bytes(8, "big"))
    digest.update(value)


def _result(digest: "hashlib._Hash") -> str:
    return f"sha256:{digest.hexdigest()}"


def _flow_files(root: Path) -> list[tuple[bytes, Path]]:
    try:
        root_stat = root.lstat()
    except OSError as error:
        raise FingerprintError("flow root is unavailable") from error
    if stat.S_ISLNK(root_stat.st_mode) or not stat.S_ISDIR(root_stat.st_mode):
        raise FingerprintError("flow root must be a real directory")

    files: list[tuple[bytes, Path]] = []
    pending = [root]
    while pending:
        directory = pending.pop()
        try:
            entries = list(os.scandir(directory))
        except OSError as error:
            raise FingerprintError("flow tree cannot be scanned") from error
        for entry in entries:
            path = Path(entry.path)
            if entry.is_symlink():
                raise FingerprintError("flow tree must not contain symlinks")
            if entry.is_dir(follow_symlinks=False):
                pending.append(path)
            elif entry.is_file(follow_symlinks=False):
                relative = path.relative_to(root).as_posix().encode("utf-8")
                files.append((relative, path))
            else:
                raise FingerprintError("flow tree must contain only regular files")
            if len(files) > MAX_FLOW_FILES:
                raise FingerprintError("flow tree exceeds the file-count limit")
    return sorted(files, key=lambda item: item[0])


def flow_tree_fingerprint(root: Path) -> str:
    digest = hashlib.sha256()
    _frame(digest, FLOW_DOMAIN)
    total = 0
    for relative, path in _flow_files(root):
        flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
        try:
            descriptor = os.open(path, flags)
        except OSError as error:
            raise FingerprintError("flow file cannot be opened safely") from error
        with os.fdopen(descriptor, "rb") as source:
            metadata = os.fstat(source.fileno())
            if not stat.S_ISREG(metadata.st_mode):
                raise FingerprintError("flow tree must contain only regular files")
            if metadata.st_size > MAX_FLOW_FILE_BYTES:
                raise FingerprintError("flow file exceeds the byte limit")
            total += metadata.st_size
            if total > MAX_FLOW_TOTAL_BYTES:
                raise FingerprintError("flow tree exceeds the aggregate byte limit")
            _frame(digest, relative)
            digest.update(metadata.st_size.to_bytes(8, "big"))
            observed = 0
            while chunk := source.read(1024 * 1024):
                observed += len(chunk)
                digest.update(chunk)
            if observed != metadata.st_size:
                raise FingerprintError("flow file changed while hashing")
    return _result(digest)


def configuration_fingerprint(
    environment: Mapping[str, str],
    allowlist: Sequence[str] = CONFIG_ENV_ALLOWLIST,
    *,
    require_complete: bool = False,
) -> str:
    manifest = configuration_manifest(
        environment,
        allowlist,
        require_complete=require_complete,
    )
    digest = hashlib.sha256()
    _frame(digest, CONFIG_DOMAIN)
    for name, value in sorted(manifest.items()):
        _frame(digest, name.encode("ascii"))
        _frame(digest, value.encode("utf-8"))
    return _result(digest)


def configuration_manifest(
    environment: Mapping[str, str],
    allowlist: Sequence[str] = CONFIG_ENV_ALLOWLIST,
    *,
    require_complete: bool = False,
) -> dict[str, str]:
    names = sorted(set(allowlist))
    if len(names) != len(allowlist):
        raise FingerprintError("configuration allowlist contains duplicates")
    optional_names = sorted(set(OPTIONAL_CONFIG_ENV_NAMES))
    if len(optional_names) != len(OPTIONAL_CONFIG_ENV_NAMES) or set(names).intersection(
        optional_names
    ):
        raise FingerprintError("configuration contract contains duplicates")
    manifest: dict[str, str] = {}
    missing: list[str] = []
    for name in names + optional_names:
        if (
            not ENV_NAME.fullmatch(name)
            or name in FORBIDDEN_CONFIG_NAMES
            or any(fragment in name for fragment in SENSITIVE_NAME_FRAGMENTS)
        ):
            raise FingerprintError("configuration allowlist contains an unsafe name")
        value = environment.get(name)
        if value is None:
            if require_complete and name not in OPTIONAL_CONFIG_ENV_NAMES:
                missing.append(name)
            manifest[name] = "absent"
            continue
        encoded = value.encode("utf-8")
        if len(encoded) > MAX_CONFIG_VALUE_BYTES:
            raise FingerprintError("allowlisted configuration value exceeds the byte limit")
        manifest[name] = f"present:{value}"
    if missing:
        raise FingerprintError("required effective configuration is incomplete")
    if require_complete:
        recognized = set(names) | set(optional_names) | UNHASHED_IRONCREW_ENV_NAMES
        if any(
            name.startswith("IRONCREW_") and name not in recognized
            for name in environment
        ):
            raise FingerprintError("unexpected IronCrew configuration is present")
    for name in DERIVED_PRESENCE_NAMES:
        manifest[f"{name}_PRESENT"] = "true" if environment.get(name) else "false"
    return manifest


def _unique_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for name, value in pairs:
        if name in result:
            raise FingerprintError("HITL keyring contains a duplicate key id")
        result[name] = value
    return result


def hitl_keyring_fingerprint(keys_json: str | None, active_id: str | None) -> str:
    digest = hashlib.sha256()
    _frame(digest, KEYRING_DOMAIN)
    if keys_json is None and active_id is None:
        _frame(digest, b"absent")
        return _result(digest)
    if keys_json is None or active_id is None:
        raise FingerprintError("HITL keyring and active id must be configured together")
    if len(keys_json.encode("utf-8")) > MAX_KEYRING_JSON_BYTES:
        raise FingerprintError("HITL keyring exceeds the byte limit")
    if not KEY_ID.fullmatch(active_id):
        raise FingerprintError("HITL active key id is invalid")
    try:
        parsed = json.loads(keys_json, object_pairs_hook=_unique_object)
    except (json.JSONDecodeError, FingerprintError) as error:
        raise FingerprintError("HITL keyring must be a unique-key JSON object") from error
    if not isinstance(parsed, dict) or not 1 <= len(parsed) <= MAX_KEYS:
        raise FingerprintError("HITL keyring has an invalid key count")

    records: list[tuple[str, str]] = []
    seen_material: set[str] = set()
    for key_id, encoded in parsed.items():
        if not KEY_ID.fullmatch(key_id) or not isinstance(encoded, str):
            raise FingerprintError("HITL keyring contains an invalid entry")
        try:
            raw = bytearray(base64.b64decode(encoded, validate=True))
        except (ValueError, base64.binascii.Error) as error:
            raise FingerprintError("HITL keyring contains invalid key material") from error
        try:
            if len(raw) != KEY_BYTES or base64.b64encode(raw).decode("ascii") != encoded:
                raise FingerprintError("HITL keys must be canonical 32-byte base64")
            material_fingerprint = hashlib.sha256(raw).hexdigest()
        finally:
            raw[:] = b"\0" * len(raw)
        if material_fingerprint in seen_material:
            raise FingerprintError("HITL keyring contains duplicate key material")
        seen_material.add(material_fingerprint)
        records.append((key_id, material_fingerprint))

    by_id = dict(records)
    if active_id not in by_id:
        raise FingerprintError("HITL active key id is unavailable")
    _frame(digest, b"present")
    for key_id, material_fingerprint in sorted(records):
        _frame(digest, f"{key_id}={material_fingerprint}".encode("ascii"))
    _frame(digest, active_id.encode("ascii"))
    _frame(digest, by_id[active_id].encode("ascii"))
    return _result(digest)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    flow_parser = subparsers.add_parser("flow")
    flow_parser.add_argument("root", type=Path)
    subparsers.add_parser("environment")
    args = parser.parse_args()
    try:
        if args.command == "flow":
            print(flow_tree_fingerprint(args.root))
        else:
            config_manifest = configuration_manifest(
                os.environ,
                require_complete=True,
            )
            print(
                json.dumps(
                    {
                        "config_fingerprint": configuration_fingerprint(
                            os.environ,
                            require_complete=True,
                        ),
                        "effective_config": config_manifest,
                        "hitl_keyring_fingerprint": hitl_keyring_fingerprint(
                            os.environ.get("IRONCREW_HITL_ENCRYPTION_KEYS"),
                            os.environ.get("IRONCREW_HITL_ACTIVE_KEY_ID"),
                        ),
                    },
                    sort_keys=True,
                    separators=(",", ":"),
                )
            )
    except FingerprintError as error:
        parser.error(str(error))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
