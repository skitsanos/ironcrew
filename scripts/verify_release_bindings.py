#!/usr/bin/env python3
"""Bind a validated image receipt to authorized source and binary inputs."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

from release_image_receipt import ReceiptError, parse_binary, sha256_file, verify_dockerfile_base


def verify(
    receipt_path: Path,
    commit_sha: str,
    source_date_epoch: int,
    dockerfile: Path,
    binary_paths: list[tuple[str, str, Path]],
) -> None:
    document = json.loads(receipt_path.read_text(encoding="utf-8"))
    if document.get("commit_sha") != commit_sha:
        raise ReceiptError("receipt commit does not match the authorized tag commit")
    if document.get("source_date_epoch") != source_date_epoch:
        raise ReceiptError("receipt epoch does not match the authorized tag commit")
    docker = document.get("dockerfile")
    base = document.get("base_image")
    if not isinstance(docker, dict) or not isinstance(base, dict):
        raise ReceiptError("validated receipt source records are missing")
    verify_dockerfile_base(dockerfile, base.get("reference"), base.get("index_digest"))
    if docker.get("sha256") != sha256_file(dockerfile):
        raise ReceiptError("Dockerfile SHA-256 mismatch")
    records = document.get("binary_artifacts")
    supplied = sorted(binary_paths, key=lambda item: item[0])
    if not isinstance(records, list) or [
        (item.get("platform"), item.get("filename")) for item in records
    ] != [(platform, filename) for platform, filename, _ in supplied]:
        raise ReceiptError("verified binary inputs must match the canonical receipt records")
    for record, (_, _, path) in zip(records, supplied, strict=True):
        if record.get("sha256") != sha256_file(path):
            raise ReceiptError(f"binary SHA-256 mismatch for {record.get('platform')}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--receipt", required=True, type=Path)
    parser.add_argument("--commit-sha", required=True)
    parser.add_argument("--source-date-epoch", required=True, type=int)
    parser.add_argument("--dockerfile", required=True, type=Path)
    parser.add_argument("--binary", action="append", required=True, type=parse_binary)
    args = parser.parse_args()
    try:
        verify(args.receipt, args.commit_sha, args.source_date_epoch, args.dockerfile, args.binary)
    except (OSError, json.JSONDecodeError, ReceiptError) as error:
        print(f"release image bindings: {error}", file=sys.stderr)
        return 1
    print(f"Release image bindings verified for {args.receipt}.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
