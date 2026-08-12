#!/usr/bin/env python3
"""Create or verify the strict IronCrew release-image receipt (offline)."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
import tarfile
from pathlib import Path
from typing import Any

HEX = re.compile(r"^[0-9a-f]{64}$")
COMMIT = re.compile(r"^(?:[0-9a-f]{40}|[0-9a-f]{64})$")
DIGEST = re.compile(r"^sha256:([0-9a-f]{64})$")
TAG = re.compile(r"^v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)$")
PLATFORMS = ("linux/amd64", "linux/arm64")
MAX_JSON_BYTES = 16 * 1024 * 1024
MAX_ARCHIVE_BYTES = 512 * 1024 * 1024
MAX_ARCHIVE_MEMBERS = 1024
ROOT_KEYS = {
    "schema_version", "artifact_kind", "tag", "commit_sha", "source_date_epoch",
    "platforms", "dockerfile", "base_image", "binary_artifacts", "oci_archive", "builder",
}


class ReceiptError(ValueError):
    pass


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def exact_keys(value: Any, keys: set[str], label: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != keys:
        raise ReceiptError(f"{label} must contain exactly: {', '.join(sorted(keys))}")
    return value


def digest_hex(value: Any, label: str) -> str:
    if not isinstance(value, str) or not DIGEST.fullmatch(value):
        raise ReceiptError(f"{label} must be a lowercase sha256 digest")
    return value


def bare_hex(value: Any, label: str) -> str:
    if not isinstance(value, str) or not HEX.fullmatch(value):
        raise ReceiptError(f"{label} must be 64 lowercase hexadecimal characters")
    return value


def archive_data(path: Path) -> tuple[str, list[dict[str, str]]]:
    if not path.is_file():
        raise ReceiptError(f"OCI archive is missing: {path}")
    if path.stat().st_size > MAX_ARCHIVE_BYTES:
        raise ReceiptError(f"OCI archive exceeds {MAX_ARCHIVE_BYTES} bytes")
    with tarfile.open(path, "r:") as archive:
        members = {}
        for member_count, member in enumerate(archive, start=1):
            if member_count > MAX_ARCHIVE_MEMBERS:
                raise ReceiptError(f"OCI archive exceeds {MAX_ARCHIVE_MEMBERS} members")
            if not member.isfile():
                continue
            if member.name in members:
                raise ReceiptError(f"duplicate OCI archive member: {member.name}")
            members[member.name] = member

        def read(name: str) -> bytes:
            member = members.get(name)
            if member is None:
                raise ReceiptError(f"OCI archive member is missing: {name}")
            if member.size > MAX_JSON_BYTES:
                raise ReceiptError(f"OCI JSON member exceeds {MAX_JSON_BYTES} bytes: {name}")
            handle = archive.extractfile(member)
            if handle is None:
                raise ReceiptError(f"cannot read OCI archive member: {name}")
            return handle.read()

        try:
            layout = json.loads(read("oci-layout"))
            index_bytes = read("index.json")
            index = json.loads(index_bytes)
        except (json.JSONDecodeError, UnicodeDecodeError) as error:
            raise ReceiptError(f"invalid OCI JSON: {error}") from error
        if layout != {"imageLayoutVersion": "1.0.0"}:
            raise ReceiptError("OCI layout version must be exactly 1.0.0")
        if index.get("schemaVersion") != 2 or not isinstance(index.get("manifests"), list):
            raise ReceiptError("OCI layout index must contain a schemaVersion 2 manifest list")
        if len(index["manifests"]) != 1:
            raise ReceiptError("OCI layout index must point to exactly one image index")
        image_descriptor = index["manifests"][0]
        if image_descriptor.get("mediaType") != "application/vnd.oci.image.index.v1+json":
            raise ReceiptError("OCI layout must point to a multi-platform image index")
        image_index_digest = digest_hex(image_descriptor.get("digest"), "image index digest")
        image_index_bytes = read(f"blobs/sha256/{image_index_digest.removeprefix('sha256:')}")
        if sha256_bytes(image_index_bytes) != image_index_digest.removeprefix("sha256:"):
            raise ReceiptError("image index blob digest mismatch")
        try:
            image_index = json.loads(image_index_bytes)
        except json.JSONDecodeError as error:
            raise ReceiptError("invalid multi-platform image index JSON") from error
        if image_index.get("schemaVersion") != 2 or not isinstance(image_index.get("manifests"), list):
            raise ReceiptError("multi-platform image index must use schemaVersion 2")

        manifests: list[dict[str, str]] = []
        for descriptor in image_index["manifests"]:
            platform = descriptor.get("platform")
            if not isinstance(platform, dict):
                raise ReceiptError("every OCI index descriptor must identify a platform")
            platform_name = f"{platform.get('os')}/{platform.get('architecture')}"
            if platform_name not in PLATFORMS:
                raise ReceiptError(f"unexpected OCI platform: {platform_name}")
            manifest_digest = digest_hex(descriptor.get("digest"), "manifest digest")
            manifest_bytes = read(f"blobs/sha256/{manifest_digest.removeprefix('sha256:')}")
            if sha256_bytes(manifest_bytes) != manifest_digest.removeprefix("sha256:"):
                raise ReceiptError(f"manifest blob digest mismatch for {platform_name}")
            try:
                manifest = json.loads(manifest_bytes)
            except json.JSONDecodeError as error:
                raise ReceiptError(f"invalid manifest JSON for {platform_name}") from error
            config_digest = digest_hex(manifest.get("config", {}).get("digest"), "config digest")
            config_bytes = read(f"blobs/sha256/{config_digest.removeprefix('sha256:')}")
            if sha256_bytes(config_bytes) != config_digest.removeprefix("sha256:"):
                raise ReceiptError(f"config blob digest mismatch for {platform_name}")
            try:
                config = json.loads(config_bytes)
            except json.JSONDecodeError as error:
                raise ReceiptError(f"invalid config JSON for {platform_name}") from error
            if config.get("os") != platform.get("os") or config.get("architecture") != platform.get("architecture"):
                raise ReceiptError(f"config platform mismatch for {platform_name}")
            manifests.append({"platform": platform_name, "digest": manifest_digest, "config_digest": config_digest})

    manifests.sort(key=lambda item: item["platform"])
    if [item["platform"] for item in manifests] != list(PLATFORMS):
        raise ReceiptError("OCI archive must contain exactly linux/amd64 and linux/arm64 once")
    return image_index_digest, manifests


def verify_dockerfile_base(path: Path, reference: str, digest: str) -> None:
    lines = path.read_text(encoding="utf-8").splitlines()
    from_lines = [line.strip() for line in lines if line.strip().upper().startswith("FROM ")]
    if from_lines != [f"FROM {reference}@{digest}"]:
        raise ReceiptError("Dockerfile must use exactly the declared digest-pinned base image")


def validate(receipt: Any, receipt_path: Path, archive_path: Path, expected_tag: str | None) -> None:
    root = exact_keys(receipt, ROOT_KEYS, "receipt")
    if type(root["schema_version"]) is not int or root["schema_version"] != 1:
        raise ReceiptError("unsupported receipt schema")
    if root["artifact_kind"] != "ironcrew-release-image":
        raise ReceiptError("unsupported receipt schema or artifact kind")
    if not isinstance(root["tag"], str) or not TAG.fullmatch(root["tag"]):
        raise ReceiptError("tag must be a stable vX.Y.Z tag")
    if expected_tag is not None and root["tag"] != expected_tag:
        raise ReceiptError(f"receipt tag {root['tag']} does not match expected tag {expected_tag}")
    if not isinstance(root["commit_sha"], str) or not COMMIT.fullmatch(root["commit_sha"]):
        raise ReceiptError("commit_sha must be a full lowercase Git object ID")
    if type(root["source_date_epoch"]) is not int or root["source_date_epoch"] < 1:
        raise ReceiptError("source_date_epoch must be a positive integer")
    if root["platforms"] != list(PLATFORMS):
        raise ReceiptError("platforms must be the canonical two-platform list")

    dockerfile = exact_keys(root["dockerfile"], {"path", "sha256"}, "dockerfile")
    if dockerfile["path"] != "docker/runtime.Dockerfile":
        raise ReceiptError("unexpected Dockerfile path")
    bare_hex(dockerfile["sha256"], "dockerfile.sha256")
    base = exact_keys(root["base_image"], {"reference", "index_digest"}, "base_image")
    if not isinstance(base["reference"], str) or not base["reference"] or "@" in base["reference"]:
        raise ReceiptError("base image reference must be non-empty and omit a digest")
    digest_hex(base["index_digest"], "base_image.index_digest")

    binaries = root["binary_artifacts"]
    if not isinstance(binaries, list) or len(binaries) != 2:
        raise ReceiptError("binary_artifacts must contain exactly two records")
    expected_binary_names = {
        "linux/amd64": "ironcrew-linux-amd64.tar.gz",
        "linux/arm64": "ironcrew-linux-arm64.tar.gz",
    }
    for record in binaries:
        exact_keys(record, {"platform", "filename", "sha256"}, "binary artifact")
        if expected_binary_names.get(record["platform"]) != record["filename"]:
            raise ReceiptError("binary artifact filename must be canonical for its platform")
        bare_hex(record["sha256"], "binary artifact sha256")
    if [record["platform"] for record in binaries] != list(PLATFORMS):
        raise ReceiptError("binary_artifacts must be canonically sorted by platform")

    oci = exact_keys(root["oci_archive"], {"filename", "sha256", "index_digest", "manifests"}, "oci_archive")
    expected_archive_name = f"ironcrew-{root['tag']}-linux-oci.tar"
    if oci["filename"] != expected_archive_name or archive_path.name != expected_archive_name:
        raise ReceiptError("OCI archive filename is not canonical for its tag")
    if bare_hex(oci["sha256"], "oci_archive.sha256") != sha256_file(archive_path):
        raise ReceiptError("OCI archive SHA-256 mismatch")
    actual_index, actual_manifests = archive_data(archive_path)
    if digest_hex(oci["index_digest"], "oci_archive.index_digest") != actual_index:
        raise ReceiptError("OCI index digest mismatch")
    if oci["manifests"] != actual_manifests:
        raise ReceiptError("OCI manifest receipt does not match archive content")
    exact_keys(root["builder"], {"implementation", "version"}, "builder")
    if not all(isinstance(root["builder"][key], str) and root["builder"][key] for key in ("implementation", "version")):
        raise ReceiptError("builder fields must be non-empty strings")
    if receipt_path.name != f"ironcrew-{root['tag']}-image-receipt.v1.json":
        raise ReceiptError("receipt filename is not canonical for its tag")


def parse_binary(value: str) -> tuple[str, str, Path]:
    try:
        platform, filename, path = value.split("=", 2)
    except ValueError as error:
        raise argparse.ArgumentTypeError("binary must be PLATFORM=FILENAME=PATH") from error
    if platform not in PLATFORMS:
        raise argparse.ArgumentTypeError(f"unsupported binary platform: {platform}")
    return platform, filename, Path(path)


def generate(args: argparse.Namespace) -> None:
    if not TAG.fullmatch(args.tag) or not COMMIT.fullmatch(args.commit_sha):
        raise ReceiptError("generate requires a stable tag and full lowercase commit SHA")
    binaries = sorted(args.binary, key=lambda item: item[0])
    if [item[0] for item in binaries] != list(PLATFORMS):
        raise ReceiptError("generate requires exactly one binary for each supported platform")
    digest_hex(args.base_digest, "base image digest")
    if not args.base_reference or "@" in args.base_reference:
        raise ReceiptError("base reference must be non-empty and omit the digest")
    verify_dockerfile_base(args.dockerfile, args.base_reference, args.base_digest)
    index_digest, manifests = archive_data(args.archive)
    receipt = {
        "schema_version": 1,
        "artifact_kind": "ironcrew-release-image",
        "tag": args.tag,
        "commit_sha": args.commit_sha,
        "source_date_epoch": args.source_date_epoch,
        "platforms": list(PLATFORMS),
        "dockerfile": {"path": "docker/runtime.Dockerfile", "sha256": sha256_file(args.dockerfile)},
        "base_image": {"reference": args.base_reference, "index_digest": args.base_digest},
        "binary_artifacts": [
            {"platform": platform, "filename": filename, "sha256": sha256_file(path)}
            for platform, filename, path in binaries
        ],
        "oci_archive": {
            "filename": args.archive.name,
            "sha256": sha256_file(args.archive),
            "index_digest": index_digest,
            "manifests": manifests,
        },
        "builder": {"implementation": args.builder_implementation, "version": args.builder_version},
    }
    args.receipt.write_text(json.dumps(receipt, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    validate(receipt, args.receipt, args.archive, args.tag)


def main() -> int:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)
    verify = subparsers.add_parser("verify")
    verify.add_argument("--receipt", required=True, type=Path)
    verify.add_argument("--archive", required=True, type=Path)
    verify.add_argument("--tag")
    create = subparsers.add_parser("generate")
    create.add_argument("--receipt", required=True, type=Path)
    create.add_argument("--archive", required=True, type=Path)
    create.add_argument("--tag", required=True)
    create.add_argument("--commit-sha", required=True)
    create.add_argument("--source-date-epoch", required=True, type=int)
    create.add_argument("--dockerfile", required=True, type=Path)
    create.add_argument("--base-reference", required=True)
    create.add_argument("--base-digest", required=True)
    create.add_argument("--builder-implementation", required=True)
    create.add_argument("--builder-version", required=True)
    create.add_argument("--binary", action="append", required=True, type=parse_binary)
    args = parser.parse_args()
    try:
        if args.command == "generate":
            generate(args)
        else:
            receipt = json.loads(args.receipt.read_text(encoding="utf-8"))
            validate(receipt, args.receipt, args.archive, args.tag)
    except (OSError, json.JSONDecodeError, ReceiptError) as error:
        print(f"release image receipt: {error}", file=sys.stderr)
        return 1
    print(f"Release image receipt {args.command} passed for {args.receipt}.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
